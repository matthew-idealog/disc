use crate::config::model::{is_reserved_server_alias, reserved_server_aliases_label, AppConfig, ServerConfig};
use crate::config::store::ConfigStore;
use crate::media_controls::{MediaIntegration, MediaKeyCommand, MediaPlaybackStatus, NowPlaying};
use crate::playback::engine::{PlaybackAudioStatus, PlaybackEngine, PlaybackState, PlaybackTrack};
use crate::subsonic::client::{DownloadBinary, QueueTrack, ResultKind, SearchResultItem, SubsonicClient};
use anyhow::Result;
use std::io::BufRead;
#[cfg(unix)]
use std::os::unix::io::{FromRawFd, IntoRawFd};
#[cfg(windows)]
use std::os::windows::io::{FromRawHandle, IntoRawHandle};
use serde::{Deserialize, Serialize};
use rand::seq::SliceRandom;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
    Terminal,
};
use std::{collections::{HashMap, HashSet}, fs, io, path::{Path, PathBuf}, sync::{mpsc, Arc}, time::{Duration, Instant, SystemTime, UNIX_EPOCH}};
use unicode_width::UnicodeWidthStr;

const BUILD_LABEL: &str = "0.1.0-rc.21";
const LAST_SESSION_NAME: &str = "last-session";
const DEFAULT_RECENT_ALBUM_COUNT: usize = 60;
const DEFAULT_RANDOM_ALBUM_COUNT: usize = 60;
const DEFAULT_RANDOM_TRACK_COUNT: usize = 60;
const QUEUE_SELECTION_PLAYBACK_GRACE_MS: u64 = 120;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchQueueAction {
    AppendPlay,
    ReplacePlay,
}

struct PendingSearch {
    command: String,
    started_at: Instant,
    timeout_seconds: u64,
    per_server: bool,
    force_queue_view: bool,
    receiver: mpsc::Receiver<SearchTaskResult>,
    handle: tokio::task::JoinHandle<()>,
}

#[derive(Clone, Copy, Debug)]
struct PendingQueuePlayback {
    index: usize,
    due_at: Instant,
}

struct SearchTaskSuccess {
    server_alias: String,
    title: String,
    items: Vec<SearchResultItem>,
    multi_server: bool,
    errors: Vec<String>,
    queue_action: Option<SearchQueueAction>,
}

type SearchTaskResult = std::result::Result<SearchTaskSuccess, String>;

pub async fn run_tui(store: ConfigStore) -> Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;
    let runtime_stderr_rx = start_runtime_stderr_capture();

    let mut app = AppState::new(store.load()?, store);
    app.runtime_stderr_capture = runtime_stderr_rx;
    let result = run_loop(&mut terminal, &mut app).await;
    app.runtime_stderr_capture = None;

    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut AppState,
) -> Result<()> {
    loop {
        drain_runtime_stderr_messages(app);
        handle_playback_events(app)?;
        handle_pending_search(app).await?;
        handle_deferred_queue_playback(app)?;
        handle_media_control_events(app)?;
        sync_media_controls(app);
        terminal.draw(|f| draw_ui(f, app))?;

        if event::poll(Duration::from_millis(100))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch.to_ascii_lowercase() == 'c' => {
                        app.clear_queue();
                    }
                    KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch.to_ascii_lowercase() == 'b' => {
                        handle_back_command(app);
                    }
                    KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && key.modifiers.contains(KeyModifiers::SHIFT) && ch.to_ascii_lowercase() == 'q' => {
                        if let Err(error) = persist_last_session(app, false) {
                            app.push_message(format!("Warning: could not save last session: {}", error));
                        }
                        break;
                    }
                    KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch.to_ascii_lowercase() == 'm' => {
                        handle_mute_toggle_command(app);
                    }
                    KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch.to_ascii_lowercase() == 'q' => {
                        app.show_queue_messages();
                    }
                    KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch.to_ascii_lowercase() == 'p' => {
                        app.show_queue_messages();
                    }
                    KeyCode::Char('+') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Err(error) = handle_volume_value(app, "+5") {
                            app.push_message(format!("Error: {}", error));
                        }
                    }
                    KeyCode::Char('=') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Err(error) = handle_volume_value(app, "+5") {
                            app.push_message(format!("Error: {}", error));
                        }
                    }
                    KeyCode::Char('-') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        if let Err(error) = handle_volume_value(app, "-5") {
                            app.push_message(format!("Error: {}", error));
                        }
                    }
                    KeyCode::Esc => {
                        if app.wizard.is_some() {
                            app.wizard = None;
                            app.push_message("Server setup cancelled.");
                        } else {
                            break;
                        }
                    }
                    KeyCode::Enter if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        handle_mute_toggle_command(app);
                    }
                    KeyCode::Enter => {
                        let command = app.input.trim().to_string();
                        if app.wizard.is_none() {
                            app.commit_input_to_history(&command);
                        } else {
                            app.history_index = None;
                            app.history_draft.clear();
                        }
                        app.clear_input();
                        if let Err(error) = handle_enter(app, &command).await {
                            app.push_message(format!("Error: {}", error));
                        }
                        if app.quit_requested {
                            break;
                        }
                    }
                    KeyCode::Up => app.history_up(),
                    KeyCode::Down => app.history_down(),
                    KeyCode::PageDown if app.selection_context == Some(SelectionContext::Help) && app.input.trim().is_empty() => {
                        show_next_list_page(app);
                    }
                    KeyCode::PageUp if app.selection_context == Some(SelectionContext::Help) && app.input.trim().is_empty() => {
                        show_previous_list_page(app);
                    }
                    KeyCode::Right if !app.input.is_empty() => app.input_cursor_right(),
                    KeyCode::Left if !app.input.is_empty() => app.input_cursor_left(),
                    KeyCode::PageDown | KeyCode::Right => {
                        if let Err(error) = handle_playback_command(app, "next") {
                            app.push_message(format!("Error: {}", error));
                        }
                    }
                    KeyCode::PageUp | KeyCode::Left => {
                        if let Err(error) = handle_playback_command(app, "prev") {
                            app.push_message(format!("Error: {}", error));
                        }
                    }
                    KeyCode::Backspace => {
                        app.input_backspace();
                    }
                    KeyCode::Home => {
                        app.input_cursor_home();
                    }
                    KeyCode::End => {
                        app.input_cursor_end();
                    }
                    KeyCode::Char(' ') if app.wizard.is_none() && app.input.trim().is_empty() => {
                        if let Err(error) = handle_playback_command(app, "p") {
                            app.push_message(format!("Error: {}", error));
                        }
                    }
                    KeyCode::Char(']') if app.input.trim().is_empty() => show_next_list_page(app),
                    KeyCode::Char('[') if app.input.trim().is_empty() => show_previous_list_page(app),
                    KeyCode::Char(ch) if app.wizard.is_none()
                        && (app.selection_context.is_none() || app.selection_context == Some(SelectionContext::Help))
                        && app.input.trim().is_empty()
                        && ch.is_ascii_digit()
                        && ch != '0' =>
                    {
                        if let Some(number) = ch.to_digit(10).map(|value| value as usize) {
                            if app.selection_context == Some(SelectionContext::Help) {
                                let _ = show_numbered_help_page(app, &number.to_string());
                            } else {
                                show_home_help_topic(app, number);
                            }
                        }
                    }
                    KeyCode::Char(ch) => {
                        if app.history_index.is_some() {
                            app.history_index = None;
                            app.history_draft.clear();
                        }
                        app.input_insert_char(ch);
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    let _ = persist_last_session(app, false);

    Ok(())
}

struct RuntimeStderrCapture {
    receiver: mpsc::Receiver<String>,
    _redirect: gag::Redirect<fs::File>,
}

fn start_runtime_stderr_capture() -> Option<RuntimeStderrCapture> {
    let (reader, writer) = os_pipe::pipe().ok()?;
    let writer_file = pipe_writer_into_file(writer);
    let redirect = gag::Redirect::stderr(writer_file).ok()?;
    let (sender, receiver) = mpsc::channel();
    let spawn_result = std::thread::Builder::new()
        .name("disc-stderr-capture".to_string())
        .spawn(move || {
            let reader = io::BufReader::new(reader);
            for line in reader.lines() {
                match line {
                    Ok(line) => {
                        let line = line.trim();
                        if !line.is_empty() {
                            let _ = sender.send(runtime_stderr_message(line));
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(format!("Runtime stderr capture stopped: {}", error));
                        break;
                    }
                }
            }
        });

    if spawn_result.is_ok() {
        Some(RuntimeStderrCapture { receiver, _redirect: redirect })
    } else {
        None
    }
}

#[cfg(unix)]
fn pipe_writer_into_file(writer: os_pipe::PipeWriter) -> fs::File {
    unsafe { fs::File::from_raw_fd(writer.into_raw_fd()) }
}

#[cfg(windows)]
fn pipe_writer_into_file(writer: os_pipe::PipeWriter) -> fs::File {
    unsafe { fs::File::from_raw_handle(writer.into_raw_handle()) }
}

fn runtime_stderr_message(line: &str) -> String {
    let lower = line.to_ascii_lowercase();
    if lower.contains("output stream") || lower.contains("audio stream") || lower.contains("device is no longer available") {
        format!(
            "Audio output warning: {}. If playback is silent or the endpoint changed, check the OS output/Volume mixer and run audio reset.",
            line
        )
    } else {
        format!("Runtime warning: {}", line)
    }
}

fn drain_runtime_stderr_messages(app: &mut AppState) {
    let mut drained = Vec::new();
    if let Some(capture) = app.runtime_stderr_capture.as_ref() {
        while let Ok(message) = capture.receiver.try_recv() {
            drained.push(message);
        }
    }
    for message in drained {
        app.push_message(message);
    }
}

fn visual_line_count(messages: &[String], width: usize) -> usize {
    let usable_width = width.max(1);

    messages
        .iter()
        .map(|message| {
            let logical_lines: Vec<&str> = if message.is_empty() {
                vec![""]
            } else {
                message.split('\n').collect()
            };

            logical_lines
                .iter()
                .map(|line| {
                    let display_width = UnicodeWidthStr::width(*line);
                    let full_lines = display_width / usable_width;
                    let partial_line = usize::from(display_width % usable_width != 0 || display_width == 0);
                    full_lines + partial_line
                })
                .sum::<usize>()
        })
        .sum()
}

fn draw_ui(frame: &mut ratatui::Frame, app: &mut AppState) {
    let theme = app.theme();
    let status_height = if app.queue_context_label().is_some() { 8 } else { 7 };
    let log_height = if app.config.advanced_status { 8 } else { 5 };
    let constraints = if app.config.messages_visible {
        vec![
            Constraint::Length(status_height),
            Constraint::Length(3),
            Constraint::Min(8),
            Constraint::Length(log_height),
        ]
    } else {
        vec![
            Constraint::Length(status_height),
            Constraint::Length(3),
            Constraint::Min(8),
        ]
    };
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(frame.size());

    let main_inner_height = chunks[2].height.saturating_sub(2) as usize;
    app.update_adaptive_page_sizes(main_inner_height);

    let status = Paragraph::new(status_panel_lines(app))
        .style(theme.block_style())
        .block(Block::default().title("DISC").borders(Borders::ALL).border_style(theme.block_style()))
        .wrap(Wrap { trim: false });

    let input_title = match &app.wizard {
        Some(wizard) => format!("Input ({})", wizard.step_label()),
        None => "Command".to_string(),
    };
    let input_text = match &app.wizard {
        Some(wizard) if matches!(wizard.step, WizardStep::Password) => "*".repeat(app.input.chars().count()),
        _ => app.input.clone(),
    };
    let input = Paragraph::new(input_text.as_str())
        .style(Style::default().fg(theme.fg).bg(theme.panel))
        .block(Block::default().title(input_title).borders(Borders::ALL).border_style(theme.block_style()));

    let main_view = Paragraph::new(active_view_lines(app))
        .style(theme.block_style())
        .block(Block::default().title(active_view_title(app)).borders(Borders::ALL).border_style(theme.block_style()))
        .wrap(Wrap { trim: false });

    frame.render_widget(status, chunks[0]);
    frame.render_widget(input, chunks[1]);
    frame.render_widget(main_view, chunks[2]);

    if app.config.messages_visible {
        let debug_title = if app.config.advanced_status {
            "Messages / verbose status"
        } else {
            "Messages"
        };
        let messages = Paragraph::new(message_log_lines(app, log_height.saturating_sub(2) as usize))
            .style(theme.block_style())
            .block(Block::default().title(debug_title).borders(Borders::ALL).border_style(theme.block_style()))
            .wrap(Wrap { trim: false });
        frame.render_widget(messages, chunks[3]);
    }

    let inner_width = chunks[1].width.saturating_sub(2) as usize;
    let cursor_col = match &app.wizard {
        Some(wizard) if matches!(wizard.step, WizardStep::Password) => app.input.chars().take(app.input_cursor_chars()).count(),
        _ => app.input_prefix_display_width(),
    }.min(inner_width);
    frame.set_cursor(chunks[1].x + 1 + cursor_col as u16, chunks[1].y + 1);
}

fn status_panel_lines(app: &AppState) -> Vec<Line<'static>> {
    let theme = app.theme();
    let label_style = status_title_style(app);
    let value_style = Style::default().fg(theme.fg).bg(theme.panel);
    let missing_value = Style::default().fg(theme.dim).bg(theme.panel);

    match &app.playback {
        Some(engine) => {
            let snapshot = engine.view_snapshot();
            let state = playback_state_label(snapshot.state);
            let status = if snapshot.state == PlaybackState::Error {
                if snapshot.error.is_some() {
                    "error (see Messages)".to_string()
                } else {
                    "error".to_string()
                }
            } else {
                state.to_string()
            };
            let current = snapshot.current.as_ref();
            let track_title = current
                .map(|track| track.title.clone())
                .unwrap_or_else(|| "none".to_string());
            let track_style = current
                .map(|track| playback_status_track_style(app, Some(track)))
                .unwrap_or(missing_value);
            let artist = current
                .map(|track| track.artist.clone())
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "-".to_string());
            let album = current
                .map(|track| track.album.clone())
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "-".to_string());
            let position = match snapshot.duration_ms {
                Some(duration) => format!("{} / {}", format_ms(snapshot.position_ms), format_ms(duration)),
                None => format_ms(snapshot.position_ms),
            };

            let mut lines = vec![
                Line::from(vec![
                    Span::styled("Playback status: ", label_style),
                    Span::styled(status, value_style),
                ]),
                {
                    let mut spans = vec![
                        Span::styled("Shuffle: ", label_style),
                        Span::styled(on_off(app.shuffle_enabled), value_style),
                        Span::raw(" | "),
                        Span::styled("Shuffle order: ", label_style),
                        Span::styled(shuffle_order_status_label(app), value_style),
                        Span::raw(" | "),
                        Span::styled("Repeat: ", label_style),
                        Span::styled(app.repeat_mode.label(), value_style),
                    ];
                    if app.config.gapless_playback || app.config.advanced_status {
                        spans.push(Span::raw(" | "));
                        spans.push(Span::styled("Gapless: ", label_style));
                        spans.push(Span::styled(on_off(app.config.gapless_playback), value_style));
                    }
                    if app.config.media_keys_enabled || app.config.advanced_status {
                        spans.push(Span::raw(" | "));
                        spans.push(Span::styled("Media keys: ", label_style));
                        spans.push(Span::styled(on_off(app.config.media_keys_enabled), value_style));
                    }
                    spans.push(Span::raw(" | "));
                    spans.push(Span::styled("Vol: ", label_style));
                    spans.push(Span::styled(snapshot.volume.to_string(), value_style));
                    Line::from(spans)
                },
                Line::from(vec![
                    Span::styled("Track: ", label_style),
                    Span::styled(track_title, track_style),
                ]),
                Line::from(vec![
                    Span::styled("Artist: ", label_style),
                    Span::styled(artist, value_style),
                    Span::raw(" | "),
                    Span::styled("Album: ", label_style),
                    Span::styled(album, value_style),
                ]),
            ];
            if let Some(label) = app.queue_context_label() {
                lines.push(Line::from(vec![
                    Span::styled("Playlist: ", label_style),
                    Span::styled(label, value_style),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("Position: ", label_style),
                Span::styled(position, value_style),
                Span::raw(" | "),
                Span::styled("Queue: ", label_style),
                Span::styled(format!("{} item(s)", app.queue.len()), value_style),
            ]));
            lines
        }
        None => {
            let mut lines = vec![
                Line::from(vec![
                    Span::styled("Playback status: ", label_style),
                    Span::styled(
                        format!(
                            "unavailable: {}",
                            app.playback_init_error
                                .as_deref()
                                .unwrap_or("audio output could not be initialised")
                        ),
                        Style::default().fg(theme.warning).bg(theme.panel),
                    ),
                ]),
                {
                    let mut spans = vec![
                        Span::styled("Shuffle: ", label_style),
                        Span::styled(on_off(app.shuffle_enabled), value_style),
                        Span::raw(" | "),
                        Span::styled("Shuffle order: ", label_style),
                        Span::styled(shuffle_order_status_label(app), value_style),
                        Span::raw(" | "),
                        Span::styled("Repeat: ", label_style),
                        Span::styled(app.repeat_mode.label(), value_style),
                    ];
                    if app.config.gapless_playback || app.config.advanced_status {
                        spans.push(Span::raw(" | "));
                        spans.push(Span::styled("Gapless: ", label_style));
                        spans.push(Span::styled(on_off(app.config.gapless_playback), value_style));
                    }
                    if app.config.media_keys_enabled || app.config.advanced_status {
                        spans.push(Span::raw(" | "));
                        spans.push(Span::styled("Media keys: ", label_style));
                        spans.push(Span::styled(on_off(app.config.media_keys_enabled), value_style));
                    }
                    spans.push(Span::raw(" | "));
                    spans.push(Span::styled("Vol: ", label_style));
                    spans.push(Span::styled("--", value_style));
                    Line::from(spans)
                },
                Line::from(vec![
                    Span::styled("Track: ", label_style),
                    Span::styled("none", missing_value),
                ]),
                Line::from(vec![
                    Span::styled("Artist: ", label_style),
                    Span::styled("-", value_style),
                    Span::raw(" | "),
                    Span::styled("Album: ", label_style),
                    Span::styled("-", value_style),
                ]),
            ];
            if let Some(label) = app.queue_context_label() {
                lines.push(Line::from(vec![
                    Span::styled("Playlist: ", label_style),
                    Span::styled(label, value_style),
                ]));
            }
            lines.push(Line::from(vec![
                Span::styled("Position: ", label_style),
                Span::styled("0:00", value_style),
                Span::raw(" | "),
                Span::styled("Queue: ", label_style),
                Span::styled(format!("{} item(s)", app.queue.len()), value_style),
            ]));
            lines
        },
    }
}

fn status_title_style(app: &AppState) -> Style {
    let theme = app.theme();
    Style::default().fg(status_title_colour(app)).bg(theme.panel)
}

fn status_title_colour(app: &AppState) -> Color {
    let theme = app.theme();
    if let Some(configured) = app.config.status_colour.as_deref() {
        if let Some(colour) = parse_status_colour(configured, &theme) {
            return colour;
        }
    }
    if theme.is_multicolour() {
        rgb(0xcc5a2a)
    } else {
        theme.accent
    }
}

fn parse_status_colour(value: &str, theme: &TerminalTheme) -> Option<Color> {
    match value.trim().to_lowercase().replace('_', "-").as_str() {
        "auto" | "default" | "style" => Some(if theme.is_multicolour() { rgb(0xcc5a2a) } else { theme.accent }),
        _ => parse_colour_token(value),
    }
}

fn status_colour_label(app: &AppState) -> String {
    app.config
        .status_colour
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or("auto")
        .to_string()
}


fn playback_status_track_style(app: &AppState, track: Option<&PlaybackTrack>) -> Style {
    let theme = app.theme();
    let mut style = Style::default().fg(theme.fg).bg(theme.panel).add_modifier(Modifier::BOLD);
    if app.config.playlist_multicolour && theme.is_multicolour() {
        let queue_index = track
            .and_then(|playing| {
                app.queue.iter().position(|queued| {
                    queued.server_alias == playing.server_alias && queued.id == playing.id
                })
            })
            .or(app.current_queue_index);
        if let Some(index) = queue_index {
            if let Some(palette_index) = queue_album_palette_index(app, index) {
                style = Style::default()
                    .fg(theme.queue_palette[palette_index % theme.queue_palette.len()])
                    .bg(theme.panel)
                    .add_modifier(Modifier::BOLD);
            }
        }
    }
    style
}

fn active_context_label(app: &AppState) -> &'static str {
    match app.selection_context {
        Some(SelectionContext::Results) => "results",
        Some(SelectionContext::Queue) => "queue",
        Some(SelectionContext::SavedQueues) => "saved queues",
        Some(SelectionContext::Recent) => "history",
        Some(SelectionContext::Messages) => "messages",
        Some(SelectionContext::Help) => "help",
        None => "home",
    }
}

fn active_view_title(app: &AppState) -> String {
    if app.wizard.is_some() {
        return "Server setup".to_string();
    }
    match app.selection_context {
        Some(SelectionContext::Results) => app.results.as_ref().map(|state| state.title.clone()).unwrap_or_else(|| "Results".to_string()),
        Some(SelectionContext::Queue) => "Play queue".to_string(),
        Some(SelectionContext::SavedQueues) => "Saved queues".to_string(),
        Some(SelectionContext::Recent) => "Playback history".to_string(),
        Some(SelectionContext::Messages) => "Messages".to_string(),
        Some(SelectionContext::Help) => app.help.as_ref().map(|state| state.title.clone()).unwrap_or_else(|| "Commands".to_string()),
        None => "Home".to_string(),
    }
}

fn active_view_lines(app: &AppState) -> Vec<Line<'static>> {
    if let Some(wizard) = &app.wizard {
        return wizard_view_lines(wizard);
    }
    match app.selection_context {
        Some(SelectionContext::Results) => app.results.as_ref().map(|state| result_view_lines(app, state)).unwrap_or_else(|| home_view_lines(app)),
        Some(SelectionContext::Queue) => queue_view_lines(app),
        Some(SelectionContext::SavedQueues) => app.saved_queues.as_ref().map(|state| saved_queue_view_lines(app, state)).unwrap_or_else(|| home_view_lines(app)),
        Some(SelectionContext::Recent) => recent_view_lines(app),
        Some(SelectionContext::Messages) => messages_view_lines(app),
        Some(SelectionContext::Help) => help_view_lines(app),
        None => home_view_lines(app),
    }
}

fn wizard_view_lines(wizard: &ServerWizard) -> Vec<Line<'static>> {
    vec![
        Line::from("Server setup wizard"),
        Line::from(format!("Current step: {}", wizard.step_label())),
        Line::from("Enter a value in the command input above and press Enter."),
        Line::from("Press Esc to cancel setup."),
    ]
}

fn home_view_lines(app: &AppState) -> Vec<Line<'static>> {
    let theme = app.theme();
    let command_style = help_command_style(app);
    let title_style = Style::default().fg(theme.accent).bg(theme.panel).add_modifier(Modifier::BOLD);
    let mut lines = vec![
        Line::from(Span::styled(format!("Welcome to DISC {}.", BUILD_LABEL), title_style)),
        Line::from("DISC Is a Subsonic Client: a cross-platform terminal app for Subsonic-compatible music servers."),
    ];
    if app.config.servers.is_empty() {
        lines.push(Line::from(vec![
            Span::raw("First time? Type "),
            Span::styled("add-server", command_style),
            Span::raw(" to configure your first Subsonic server."),
        ]));
    }
    lines.extend([
        Line::from("Type help or h for the help index. Type a number below to open a topic."),
        Line::from(""),
        Line::from(Span::styled("Help topics", title_style)),
    ]);
    for (number, title) in HOME_HELP_TOPICS.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(format!("{}. ", number + 1), command_style),
            Span::raw(*title),
        ]));
    }
    if app.store.session_path().exists() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![
            Span::raw("Last session is available: use "),
            Span::styled("restore-session", command_style),
            Span::raw(", "),
            Span::styled("rs", command_style),
            Span::raw(", or "),
            Span::styled("session restore", command_style),
            Span::raw("."),
        ]));
    }
    lines
}

const HOME_HELP_TOPICS: [&str; 10] = [
    "Search & results",
    "Server configuration",
    "Command routing",
    "Playback controls",
    "Playlist, session & history",
    "Downloading",
    "Appearance & styling",
    "Keyboard shortcuts",
    "Other configuration",
    "Beginner guide",
];

fn help_command_style(app: &AppState) -> Style {
    let theme = app.theme();
    let colour = if theme.is_multicolour() { theme.secondary } else { theme.accent };
    Style::default().fg(colour).bg(theme.panel)
}

fn help_heading_style(app: &AppState) -> Style {
    let theme = app.theme();
    Style::default().fg(theme.accent).bg(theme.panel).add_modifier(Modifier::BOLD)
}

fn help_line_from_markup(app: &AppState, text: &str) -> Line<'static> {
    let command_style = help_command_style(app);
    let heading_style = help_heading_style(app);
    let trimmed = text.trim_end();
    if trimmed.starts_with("# ") || trimmed.starts_with("## ") || trimmed.starts_with("### ") {
        return Line::from(Span::styled(trimmed.trim_start_matches('#').trim().to_string(), heading_style));
    }
    let mut spans = Vec::new();
    let mut current = String::new();
    let mut in_code = false;
    for ch in trimmed.chars() {
        if ch == '`' {
            if !current.is_empty() {
                let chunk = std::mem::take(&mut current);
                if in_code {
                    spans.push(Span::styled(chunk, command_style));
                } else {
                    spans.push(Span::raw(chunk));
                }
            }
            in_code = !in_code;
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        if in_code {
            spans.push(Span::styled(current, command_style));
        } else {
            spans.push(Span::raw(current));
        }
    }
    if spans.is_empty() {
        Line::from("")
    } else {
        Line::from(spans)
    }
}

fn help_index_lines() -> Vec<String> {
    let mut lines = vec![
        "# DISC help".to_string(),
        "DISC Is a Subsonic Client: a cross-platform terminal app for Subsonic-compatible music servers.".to_string(),
        "Type a number to open a topic. Use `[` / `]` or PageUp / PageDown to move between help topics.".to_string(),
        "Use `help basics`, `help beginner`, or `help 10` for a beginner guide; use `help full` or `help commands` for the complete command reference.".to_string(),
        "".to_string(),
        "## Help topics".to_string(),
    ];
    for (number, title) in HOME_HELP_TOPICS.iter().enumerate() {
        lines.push(format!("`{}` {}", number + 1, title));
    }
    lines
}

fn show_help_index(app: &mut AppState) {
    app.help = Some(HelpState::new_topic("DISC help index".to_string(), help_index_lines(), 0));
    app.selection_context = Some(SelectionContext::Help);
    app.push_message("Showing DISC help index. Type a topic number, or use [ / ] / PageUp / PageDown to move through help topics.".to_string());
}

fn show_home_help_topic(app: &mut AppState, number: usize) -> bool {
    let Some((title, lines)) = home_help_topic(number) else {
        return false;
    };
    app.help = Some(HelpState::new_topic(format!("DISC help {}: {}", number, title), lines, number));
    app.selection_context = Some(SelectionContext::Help);
    app.push_message(format!("Showing help topic {}: {}. Use [ / ] or PageUp / PageDown to move between help topics.", number, title));
    true
}

fn home_help_topic(number: usize) -> Option<(&'static str, Vec<String>)> {
    let title = *HOME_HELP_TOPICS.get(number.checked_sub(1)?)?;
    let lines: Vec<&str> = match number {
        1 => vec![
            "# Search & results",
            "Use `s <query>` for general search, `al <query>` for albums, `ar <query>` / `art <query>` for artists, and `tr <query>` for tracks.",
            "Use `rec` for recent albums, `rnd` for random albums, and `rnd t` for random tracks.",
            "Use `all s <query>` to search all configured servers at once. `all al <query>`, `all ar <query>`, `all tr <query>`, and `all pl <query>` work similarly; multi-server results show a server suffix such as `[sub]`.",
            "Add `--play`, `--p`, or `-p` to append every returned playable result to the queue and start at the first added track; add `--replace --play`, `--rp`, or `-rp` to replace the queue first.",
            "Wildcards `*` and `?` work in supported searches, for example `al haz*` or `rnd 5 g ?rock`. Matching is case-insensitive where DISC filters locally; album wildcard search also uses a bounded local fallback when server seed search misses partial terms.",
            "Select a single result with `1`. Albums/playlists usually load tracks; artists/genres navigate deeper; tracks play directly.",
            "Add several results to the play queue with `a 1 3 5-7` or just `1 3 5-7` in results-like pages. Use `play 1`, `p 1`, `p1`, `play *`, `p *`, or `p*` to append selected results and start playback at the first added track.",
            "Add `q` or `queue` at the end, such as `1 3 5-7 q`, to show the queue once after adding.",
            "Use `[` and `]` to move through help topics while viewing help; use them to page active result lists elsewhere.",
            "Use `find <text>` or `/ <text>` to filter the active list without replacing it.",
            "",
            "## Search command lookup",
            "`search` = `s`",
            "`album` = `al`",
            "`artist` = `ar` or `art`",
            "`track` = `tr`",
            "`recent` = `rec`",
            "`random` = `rnd`",
            "`genre` = `g`",
            "`playlist` / `playlists` = `pl`",
            "`all <search>` = search across every configured server",
            "`add` = `a`",
            "`queue` = `q`",
            "`back` = `b`",
            "`info` = `i`",
        ],
        2 => vec![
            "# Server configuration",
            "Use `servers` to list configured Subsonic servers and `primary` to show the primary/home server.",
            "Use `add-server`, `edit-server <target>`, and `remove-server <target>` to manage servers.",
            "A server alias is a short nickname for a server, used to make server-specific commands quicker to type, for example `sub rec` or `hel rnd 50`.",
            "Use `use <alias-or-name>` or `primary <alias-or-name>` to change the primary server.",
            "Use `ping`, `ping <target>`, or `ping all` to test connectivity.",
            "Run shell commands such as `disc server-add ...` before opening the TUI for scripted setup.",
            "Reserved short commands such as `all`, `k`, `kill`, `s`, `al`, `ar`, `tr`, `pl`, `gap`, `qf`, and `msg` cannot be used as server aliases.",
        ],
        3 => vec![
            "# Command routing",
            "Bare commands target the primary/home server when a server is needed.",
            "Prefix a command with a server alias to target that server, for example `hel rec` or `sub rnd 100`. Prefix with `all` to run supported searches across every configured server, for example `all s deep`.",
            "The command parser treats reserved aliases as commands first, so `s haz*` is always search, not a server called `s`.",
            "Current pages keep their own context: numbers act on results, queue, saved queues, history, or help depending on the active page.",
            "Use `b` / `back` to return from queue/history/saved/help style pages to the previous list when possible.",
            "Use `view home`, `view results`, `view queue`, `view messages`, or `view help` to switch pages explicitly.",
        ],
        4 => vec![
            "# Playback controls",
            "Use `play`, `pause`, `p`, or Space to control playback.",
            "Use `next` / `n` / PageDown / Right and `prev` / PageUp / Left to move through the queue. Left/Right edit the command line instead when input text is present.",
            "Use `v70`, `vol70`, `v +5`, `v -5`, or Ctrl++ / Ctrl+- to adjust volume.",
            "Use `mute` or Ctrl+M to toggle mute/unmute.",
            "Use `seek 125`, `sk 2:05`, `ff`, and `rew` to move within a track.",
            "Use `sh` for shuffle play and `so` for stable visible shuffle order. Use `unsh` to restore original order and turn shuffle off.",
            "Use `gap` / `gapless` to toggle experimental gapless preload/handoff playback.",
            "Use `audio status`, `audio devices`, and `audio reset` if Windows changes audio endpoints after RDP, Bluetooth, HDMI, USB audio, or sleep/wake.",
        ],
        5 => vec![
            "# Playlist, session & history",
            "Use `pl` / `playlists` to list server playlists and `pl <query>` to search them.",
            "Use `ps`, `pu`, `pa`, `pd`, and `pr` for server playlist save/update/add/delete/rename shortcuts.",
            "Use `sq <name>` to save the current queue locally; use `queues` to list saved queues.",
            "Use `lq <n>` to load a saved queue, `dq <n>` to delete one, and `rq <n> <new-name>` to rename one.",
            "Use `ss` / `session save` to save the current session and `rs` / `restore-session` to restore it.",
            "Use `history`, `played`, or `recent tracks` to show in-session playback history.",
            "Use `clear-session` or `session clear` to remove the saved last-session file.",
        ],
        6 => vec![
            "# Downloading",
            "Use `dl now` to download the current track, `dl <n...>` to download selected list items, or `dl *` for the current list.",
            "Downloads work from result, queue, history, and saved-queue contexts where supported.",
            "Use `download-path` / `dl-path` to show or set the download folder.",
            "Use `download-path default` to reset to the platform default download location.",
            "Use `download-overwrite` / `dl-overwrite` to show or toggle the overwrite preference.",
            "Per-command flags include `--replace`, `-replace`, `--overwrite`, `-f`, `--skip`, and `--no-overwrite`.",
            "Use `doc downloads` or `diag downloads` to test whether the configured download folder is writable.",
        ],
        7 => vec![
            "# Appearance & styling",
            "Use `sty`, `style`, or `theme` to show the active style or switch styles.",
            "Built-in styles include soft, mid, bright, multi-soft, multi-mid, and multi-bright.",
            "Use `style path` to locate the editable custom style file, `style sample` to create an example, and `style reload` after editing it.",
            "Custom styles support named colours, `ansi(208)`, `#ff8800`, `rgb(255,136,0)`, and `cmyk(0,47,100,0)` values.",
            "Use `status colour <colour>` to set top-panel static labels; `status colour auto` follows the current style.",
            "Use `pmc` to control play-queue multicolour album grouping.",
            "Use `rmc` to control optional multicolour result/saved-list rows.",
            "Use `msg` to toggle the bottom message panel and `msg log` to open the full message log.",
            "Use `verbose` or `vt` to toggle verbose hints/status.",
        ],
        8 => vec![
            "# Keyboard shortcuts",
            "Space toggles play/pause when the input is empty.",
            "PageDown or Right plays the next queue track; PageUp or Left plays the previous queue track. Left/Right move within command input when text is being edited.",
            "`[` and `]` page the active list, including help pages.",
            "Ctrl+Q shows the play queue; Ctrl+B goes back; Ctrl+M toggles mute.",
            "Ctrl+C clears the play queue and stops playback.",
            "Ctrl++ / Ctrl+= raises volume by 5; Ctrl+- lowers volume by 5.",
            "Ctrl+Shift+Q saves the last session and quits.",
            "Esc also saves the last session and exits the TUI.",
            "Use `mk on` / `media-keys on` to enable OS media keys and now-playing metadata where supported by the platform.",
            "Use `kill` or `k` to cancel the currently running background search/browse request.",
        ],
        9 => vec![
            "# Other configuration",
            "Use `qf` / `queue-follow` to toggle whether queue-affecting result commands automatically show the queue.",
            "Use `gap` / `gapless` to toggle the gapless playback preference.",
            "Use `mk` / `media-keys` to toggle OS media controls and now-playing metadata publishing.",
            "Use `msg on/off` to show or hide the bottom message panel.",
            "Use `verbose on/off` or bare `verbose` / `vt` for persistent verbose hints/status.",
            "Use `doc`, `diag`, or `doctor` to show diagnostics; add `all`, `ping`, or `downloads` for focused checks.",
            "Use `version` or `about` to show the current build and key settings.",
            "Use `quit`, `exit`, Esc, or Ctrl+Shift+Q to save the last session and exit.",
        ],
        10 => vec![
            "# Beginner guide",
            "## 1. Configure a server",
            "Run `add-server` and follow the prompts. The server alias is a short nickname used in commands, such as `sub` or `hel`.",
            "For the server URL, a full URL such as `http://myserver:4040` is best. If you enter only a host name, DISC tries common Subsonic defaults and saves the URL that responds.",
            "Use `servers` to list configured servers, `use <alias>` to choose the primary server, and `ping` or `ping all` to test connections.",
            "",
            "## 2. Choose a style",
            "Use `sty` to show the current style and `sty 4` for the default multi-soft style. Other built-ins are `sty 1` through `sty 6`.",
            "",
            "## 3. Search",
            "Use `al <query>` for albums, `tr <query>` for tracks, `pl <query>` for playlists, `rec` for recent albums, and `rnd` / `rnd t` for random albums or tracks.",
            "Prefix with a server alias, such as `sub al blue`, or use `all tr love` to search every configured server.",
            "",
            "## 4. Build a play queue",
            "From results, `add 1`, `a 1 3 5-7`, or `add *` appends tracks to the queue. Bare multi-selectors such as `1 3 5-7` also append in results pages.",
            "Append and start playback directly with `play 1`, `p 1`, `p1`, `play *`, `p *`, or `p*`.",
            "Search suffixes also work: `tr love -p` appends results and starts playback; `tr love -rp` replaces the queue first, then plays.",
            "Use `remove <n...>` / `r <n...>` to remove queue items, `move <from> <to>` to reorder, and `clear` to clear the queue.",
            "",
            "## 5. Use the play queue",
            "Use `queue` or Ctrl+Q to switch to the play queue. Page with `[` and `]` when the queue is longer than the screen.",
            "Use `next` / `n` / PageDown and `prev` / PageUp to move through the queue. Use Space or `p` to pause/resume.",
            "",
            "## 6. Shuffle and order",
            "Use `shuffle` / `sh` to toggle shuffle. Use `so` for stable visible shuffle order, and `unsh` to restore the original order and turn normal shuffle off.",
            "",
            "## 7. Save and restore",
            "DISC saves the last session on clean exit. Use `rs` / `restore-session` to reload it and `ss` / `session save` to save it manually.",
            "Use `sq <name>` to save a named local queue and `queues` / `lq <n>` to list and load saved queues.",
            "For server playlists, use `pl` to list/search playlists and the `ps`, `pu`, `pa`, `pd`, and `pr` shortcuts to save, update, add, delete, and rename playlists.",
            "",
            "## Good next commands",
            "Try `add-server`, `ping`, `sty 4`, `rec -p`, `queue`, `sh`, `sq favourites`, and `help playback`.",
        ],
        _ => return None,
    };
    Some((title, lines.into_iter().map(|line| line.to_string()).collect()))
}

fn help_topic_number_from_query(query: &str) -> Option<usize> {
    let key = query.trim().to_lowercase();
    if let Ok(number) = key.parse::<usize>() {
        if (1..=HOME_HELP_TOPICS.len()).contains(&number) {
            return Some(number);
        }
    }
    match key.as_str() {
        "search" | "results" | "result" | "browse" | "navigation" | "nav" | "random" | "recent" | "wildcard" | "wildcards" | "find" | "filter" | "sort" | "kill" | "cancel" | "info" | "details" | "star" | "stars" | "starred" | "favourites" | "favorites" | "favs" => Some(1),
        "server" | "servers" | "server configuration" | "config server" | "setup" | "primary" | "ping" | "add-server" | "edit-server" | "remove-server" => Some(2),
        "routing" | "command routing" | "route" | "aliases" | "alias" | "prefix" | "prefixes" => Some(3),
        "playback" | "controls" | "playback controls" | "audio" | "audio status" | "audio devices" | "audio reset" | "gapless" | "gap" | "shuffle" | "shuffle-order" | "repeat" | "volume" | "seek" | "skip" | "mute" => Some(4),
        "playlist" | "playlists" | "queue" | "queues" | "saved" | "saved queues" | "session" | "history" | "played" | "recent tracks" => Some(5),
        "download" | "downloads" | "downloading" | "dl" => Some(6),
        "appearance" | "styling" | "style" | "styles" | "theme" | "colour" | "color" | "status colour" | "status color" | "custom styles" | "ui" | "view" | "views" | "message" | "messages" | "msg" | "verbose" => Some(7),
        "keyboard" | "keys" | "keyboard shortcuts" | "shortcuts" => Some(8),
        "other" | "other configuration" | "configuration" | "queue-follow" | "qf" | "media" | "media keys" | "media-keys" | "mk" | "now playing" | "doctor" | "diagnostics" | "diag" | "doc" | "version" | "about" => Some(9),
        "basic" | "basics" | "beginner" | "beginners" | "beginner guide" | "getting started" | "start" | "new user" | "new users" => Some(10),
        _ => None,
    }
}

fn messages_view_lines(app: &AppState) -> Vec<Line<'static>> {
    let total = app.messages.len();
    if total == 0 {
        return vec![Line::from("No messages yet.")];
    }
    let max_lines = 80usize;
    let start = total.saturating_sub(max_lines);
    let mut lines = vec![Line::from(format!(
        "Message log | showing {}-{} of {} | cls clears messages | advanced on mirrors active pages",
        start + 1,
        total,
        total
    ))];
    for (idx, message) in app.messages[start..].iter().enumerate() {
        lines.push(Line::from(format!("{}. {}", start + idx + 1, message)));
    }
    lines
}

fn help_view_lines(app: &AppState) -> Vec<Line<'static>> {
    let Some(state) = app.help.as_ref() else {
        return vec![Line::from("No command reference is loaded. Type help or h.")];
    };
    let total = state.lines.len();
    if total == 0 {
        return vec![Line::from("No command reference lines are available.")];
    }
    let range = state.visible_range();
    let header = if let Some(topic) = state.topic_number {
        if topic == 0 {
            format!("Help index | {} topics | [ / ] or PageUp/PageDown next topic | type 1-{} to open a topic", HOME_HELP_TOPICS.len(), HOME_HELP_TOPICS.len())
        } else {
            format!("Help topic {}/{} | [ / ] or PageUp/PageDown previous/next topic | type 1-{} to jump", topic, HOME_HELP_TOPICS.len(), HOME_HELP_TOPICS.len())
        }
    } else {
        format!(
            "Showing {}-{} of {} | page {}/{} | [ / ] previous/next",
            range.start + 1,
            range.end,
            total,
            state.current_page_number(),
            state.page_count()
        )
    };
    let mut lines = vec![
        Line::from(header),
        Line::from(""),
    ];
    for line in state.lines[range].iter() {
        lines.push(help_line_from_markup(app, line));
    }
    lines
}

fn result_view_lines(app: &AppState, state: &ResultsState) -> Vec<Line<'static>> {
    let theme = app.theme();
    let total = state.items.len();
    if total == 0 {
        return vec![Line::from(format!("{}: no results.", state.title))];
    }
    let range = state.visible_range();
    let mut lines = vec![
        Line::from(format!(
            "Showing {}-{} of {} | page {}/{}",
            range.start + 1,
            range.end,
            total,
            state.current_page_number(),
            state.page_count()
        )),
        Line::from(""),
    ];
    let server_order = result_server_order(&state.items);
    for (visible_idx, item) in state.items[range.clone()].iter().enumerate() {
        let absolute_number = range.start + visible_idx + 1;
        let mut style = Style::default().fg(theme.fg).bg(theme.panel);
        if state.show_server_suffix && theme.is_multicolour() {
            let colour_index = server_order
                .iter()
                .position(|alias| alias.eq_ignore_ascii_case(&item.server_alias))
                .unwrap_or(absolute_number - 1);
            style = Style::default().fg(theme.queue_palette[colour_index % theme.queue_palette.len()]).bg(theme.panel);
        } else if app.config.result_multicolour && theme.is_multicolour() {
            style = Style::default().fg(theme.queue_palette[(absolute_number - 1) % theme.queue_palette.len()]).bg(theme.panel);
        }
        lines.push(Line::from(Span::styled(format!("{}. {}", absolute_number, app.result_item_label_for_state(item, state)), style)));
    }
    lines
}



fn cleaned_result_subtitle(subtitle: &str, server_alias: &str) -> String {
    let alias = server_alias.trim();
    if alias.is_empty() {
        return subtitle.trim().to_string();
    }
    let bracketed = format!("[{}]", alias);
    subtitle
        .split('•')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .filter(|part| !part.eq_ignore_ascii_case(alias))
        .filter(|part| !part.eq_ignore_ascii_case(&bracketed))
        .collect::<Vec<_>>()
        .join(" • ")
}

fn result_server_order(items: &[SearchResultItem]) -> Vec<String> {
    let mut aliases = Vec::new();
    for item in items {
        if !aliases.iter().any(|alias: &String| alias.eq_ignore_ascii_case(&item.server_alias)) {
            aliases.push(item.server_alias.clone());
        }
    }
    aliases
}

fn queue_view_lines(app: &AppState) -> Vec<Line<'static>> {
    if app.queue.is_empty() {
        return vec![Line::from("Queue is empty.")];
    }
    let range = app.queue_visible_range();
    let mut lines = vec![
        Line::from(format!(
            "Showing {}-{} of {} | page {}/{}",
            range.start + 1,
            range.end,
            app.queue.len(),
            app.queue_current_page_number(),
            app.queue_page_count()
        )),
        Line::from(""),
    ];
    for (visible_idx, track) in app.queue[range.clone()].iter().enumerate() {
        let idx = range.start + visible_idx;
        let marker = if Some(idx) == app.current_queue_index { ">" } else { " " };
        let mut style = Style::default().fg(app.theme().fg).bg(app.theme().panel);
        let theme = app.theme();
        if app.config.playlist_multicolour && theme.is_multicolour() {
            if let Some(palette_index) = queue_album_palette_index(app, idx) {
                style = Style::default().fg(theme.queue_palette[palette_index % theme.queue_palette.len()]).bg(theme.panel);
            }
        }
        lines.push(Line::from(Span::styled(format!("{} {}. {}", marker, idx + 1, app.track_label(track)), style)));
    }
    lines
}

fn saved_queue_view_lines(app: &AppState, state: &SavedQueueListState) -> Vec<Line<'static>> {
    let total = state.entries.len();
    if total == 0 {
        return vec![Line::from("No saved queues.")];
    }
    let range = state.visible_range();
    let mut lines = vec![
        Line::from(format!(
            "Showing {}-{} of {} | page {}/{}",
            range.start + 1,
            range.end,
            total,
            state.current_page_number(),
            state.page_count()
        )),
        Line::from(""),
    ];
    let theme = app.theme();
    for (visible_idx, entry) in state.entries[range.clone()].iter().enumerate() {
        let absolute_number = range.start + visible_idx + 1;
        let mut style = Style::default().fg(theme.fg).bg(theme.panel);
        if app.config.result_multicolour && theme.is_multicolour() {
            style = Style::default().fg(theme.queue_palette[(absolute_number - 1) % theme.queue_palette.len()]).bg(theme.panel);
        }
        lines.push(Line::from(Span::styled(format!("{}. {} ({})", absolute_number, entry.name, app.saved_queue_entry_suffix(entry)), style)));
    }
    lines
}

fn recent_view_lines(app: &AppState) -> Vec<Line<'static>> {
    if app.recent_tracks.is_empty() {
        return vec![Line::from("No playback-history tracks in this session.")];
    }
    let range = app.recent_visible_range();
    let mut lines = vec![
        Line::from(format!(
            "Showing {}-{} of {} | page {}/{}",
            range.start + 1,
            range.end,
            app.recent_tracks.len(),
            app.recent_current_page_number(),
            app.recent_page_count()
        )),
        Line::from(""),
    ];
    for (visible_idx, track) in app.recent_tracks[range.clone()].iter().enumerate() {
        let idx = range.start + visible_idx;
        lines.push(Line::from(format!("{}. {}", idx + 1, app.track_label(track))));
    }
    lines
}

fn message_log_lines(app: &AppState, max_lines: usize) -> Vec<Line<'static>> {
    let mut lines = bottom_status_lines(app);
    let max_lines = max_lines.max(1);
    if lines.len() >= max_lines {
        lines.truncate(max_lines);
        return lines;
    }

    let remaining = max_lines - lines.len();
    let start = app.messages.len().saturating_sub(remaining);
    lines.extend(
        app.messages[start..]
            .iter()
            .map(|message| themed_console_line(app, message)),
    );
    lines
}

fn bottom_status_lines(app: &AppState) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    if let Some(line) = pending_search_line(app) {
        lines.push(line);
    }

    if !app.config.advanced_status {
        return lines;
    }

    let theme = app.theme();
    let primary = app
        .config
        .primary_server()
        .map(|server| format!("{} [{}]", server.name, server.alias))
        .unwrap_or_else(|| "none".to_string());
    lines.push(Line::from(vec![
        Span::styled("Primary: ", Style::default().fg(theme.secondary).bg(theme.panel)),
        Span::styled(primary, Style::default().fg(theme.fg).bg(theme.panel)),
        Span::raw(" | "),
        Span::styled("Style: ", Style::default().fg(theme.secondary).bg(theme.panel)),
        Span::styled(theme.label(), Style::default().fg(theme.fg).bg(theme.panel)),
        Span::raw(" | "),
        Span::styled("Build: ", Style::default().fg(theme.secondary).bg(theme.panel)),
        Span::styled(BUILD_LABEL, Style::default().fg(theme.fg).bg(theme.panel)),
    ]));

    if let Some(action_hint) = active_action_hint(app) {
        lines.push(Line::from(action_hint));
    }

    lines.push(Line::from(format!(
        "Debug: view={} | servers={} | results={} | queue-page={} | saved={} | history={} | config={}",
        active_context_label(app),
        app.config.servers.len(),
        app.results
            .as_ref()
            .map(|results| format!("{}/{}", results.current_page_number(), results.page_count()))
            .unwrap_or_else(|| "none".to_string()),
        format!("{}/{}", app.queue_current_page_number(), app.queue_page_count()),
        app.saved_queues.as_ref().map(|state| state.entries.len()).unwrap_or(0),
        app.recent_tracks.len(),
        app.store.path().display()
    )));
    lines.push(Line::from(format!(
        "Flags: verbose={} | pmc={} | rmc={} | download overwrite={} | repeat={} | shuffle={} | shuffle-order={} | gapless={}",
        on_off(app.config.advanced_status),
        on_off(app.config.playlist_multicolour),
        on_off(app.config.result_multicolour),
        on_off(app.config.download_overwrite),
        app.repeat_mode.label(),
        on_off(app.shuffle_enabled),
        shuffle_order_status_label(app),
        on_off(app.config.gapless_playback)
    )));

    lines
}

fn pending_search_line(app: &AppState) -> Option<Line<'static>> {
    let pending = app.pending_search.as_ref()?;
    let theme = app.theme();
    let elapsed = pending.started_at.elapsed().as_secs();
    let suffix = if pending.per_server { "/server" } else { "" };
    Some(Line::from(vec![
        Span::styled("Search: ", Style::default().fg(theme.secondary).bg(theme.panel)),
        Span::styled("running", Style::default().fg(theme.fg).bg(theme.panel)),
        Span::raw(" | "),
        Span::styled(format!("{}s/{}s{}", elapsed, pending.timeout_seconds, suffix), Style::default().fg(theme.fg).bg(theme.panel)),
        Span::raw(" | "),
        Span::styled(pending.command.clone(), Style::default().fg(theme.fg).bg(theme.panel)),
        Span::raw(" | "),
        Span::styled("kill/k cancels", Style::default().fg(theme.secondary).bg(theme.panel)),
    ]))
}

fn active_action_hint(app: &AppState) -> Option<String> {
    match app.selection_context {
        Some(SelectionContext::Results) => app.results.as_ref().map(|state| app.result_action_hint(state)),
        Some(SelectionContext::Queue) if !app.queue.is_empty() => Some(
            "Actions: <n> selects | PageUp/PageDown prev/next | Left/Right prev/next only when command input is empty | remove/r<n...> | move <from> <to> | shuffle/sh toggles | so stable order | dedupe | clear-played | clear-upcoming.".to_string(),
        ),
        Some(SelectionContext::SavedQueues) if app.saved_queues.as_ref().map(|state| !state.entries.is_empty()).unwrap_or(false) => Some(
            "Actions: <n>/lq <n> loads | a<n> appends | dq <n> deletes | rq <n> <new-name> renames | [ / ] page.".to_string(),
        ),
        Some(SelectionContext::Recent) if !app.recent_tracks.is_empty() => Some(
            "Actions: <n> plays | a<n...> appends | dl <n...> downloads | star <n...> updates favourites | history clear clears.".to_string(),
        ),
        Some(SelectionContext::Messages) => Some("Actions: cls clears messages | view home returns to home | verbose/vt toggles verbose status.".to_string()),
        Some(SelectionContext::Help) => Some("Actions: [ / ] page commands | page <n> jumps | view home returns to home.".to_string()),
        _ => None,
    }
}

fn on_off(value: bool) -> &'static str {
    if value { "on" } else { "off" }
}

fn shuffle_order_status_label(app: &AppState) -> &'static str {
    if !app.shuffle_order_enabled {
        "off"
    } else if app.shuffle_enabled {
        "on"
    } else {
        "standby"
    }
}

fn adaptive_page_size(main_inner_height: usize) -> usize {
    // Leave room for the list heading and its blank spacer. Action hints live in the verbose bottom panel.
    main_inner_height.saturating_sub(2).max(1)
}

fn update_page_window(page_start: &mut usize, page_size: &mut usize, new_page_size: usize, len: usize) {
    let new_page_size = new_page_size.max(1);
    if *page_size != new_page_size {
        *page_size = new_page_size;
        if len == 0 {
            *page_start = 0;
        } else {
            let current_item = (*page_start).min(len - 1);
            *page_start = (current_item / new_page_size) * new_page_size;
        }
    }

    if len == 0 {
        *page_start = 0;
    } else if *page_start >= len {
        *page_start = ((len - 1) / new_page_size) * new_page_size;
    }
}

fn themed_console_line(app: &AppState, message: &str) -> Line<'static> {
    let theme = app.theme();
    let lower = message.to_lowercase();
    let mut style = Style::default().fg(theme.fg).bg(theme.panel);

    if lower.starts_with("error:")
        || lower.contains("playback error")
        || lower.contains("failed")
        || lower.contains("unable")
        || lower.contains("unknown")
        || lower.contains("no such")
    {
        style = Style::default().fg(theme.danger).bg(theme.panel);
    } else if lower.contains("warning")
        || lower.contains("suppressed")
        || lower.contains("already at")
        || lower.contains("skipped")
        || lower.contains("empty")
    {
        style = Style::default().fg(theme.warning).bg(theme.panel);
    } else if lower.starts_with("actions:")
        || lower.starts_with("use ")
        || lower.starts_with("numbers are")
        || lower.starts_with("saved queue numbers")
    {
        style = Style::default().fg(theme.dim).bg(theme.panel);
    } else if lower.contains("playing:")
        || lower.contains("playback resumed")
        || lower.contains("queue replaced")
        || lower.contains("added ")
        || lower.contains("loaded saved queue")
    {
        style = Style::default().fg(theme.accent).bg(theme.panel);
    }

    if app.config.playlist_multicolour && theme.is_multicolour() {
        if let Some(queue_index) = leading_queue_number(message).and_then(|number| number.checked_sub(1)) {
            if let Some(palette_index) = queue_album_palette_index(app, queue_index) {
                style = Style::default().fg(theme.queue_palette[palette_index % theme.queue_palette.len()]).bg(theme.panel);
            }
        }
    }

    if app.config.result_multicolour && theme.is_multicolour() {
        if let Some(result_number) = leading_plain_number(message) {
            let palette_index = result_number.saturating_sub(1) % theme.queue_palette.len();
            style = Style::default().fg(theme.queue_palette[palette_index]).bg(theme.panel);
        }
    }

    Line::from(Span::styled(message.to_string(), style))
}

fn leading_queue_number(message: &str) -> Option<usize> {
    if !(message.starts_with("> ") || message.starts_with("  ")) {
        return None;
    }
    let trimmed = message.trim_start_matches(|ch| ch == '>' || ch == ' ');
    let digits: String = trimmed.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    if !trimmed[digits.len()..].starts_with('.') {
        return None;
    }
    digits.parse::<usize>().ok().filter(|number| *number > 0)
}

fn leading_plain_number(message: &str) -> Option<usize> {
    if message.starts_with("> ") || message.starts_with("  ") {
        return None;
    }
    let digits: String = message.chars().take_while(|ch| ch.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    if !message[digits.len()..].starts_with('.') {
        return None;
    }
    digits.parse::<usize>().ok().filter(|number| *number > 0)
}

fn queue_album_palette_index(app: &AppState, queue_index: usize) -> Option<usize> {
    let target = app.queue.get(queue_index)?;
    let target_key = queue_album_key(target);
    let mut album_order: Vec<String> = Vec::new();

    for track in app.queue.iter().take(queue_index + 1) {
        let key = queue_album_key(track);
        if !album_order.iter().any(|seen| seen == &key) {
            album_order.push(key);
        }
    }

    album_order.iter().position(|seen| seen == &target_key)
}

fn queue_album_key(track: &QueueTrack) -> String {
    format!("{}::{}", track.server_alias.to_lowercase(), track.album.to_lowercase())
}

#[derive(Clone)]
struct ResultsState {
    server_alias: String,
    title: String,
    items: Vec<SearchResultItem>,
    page_start: usize,
    page_size: usize,
    show_server_suffix: bool,
}

impl ResultsState {
    fn new(server_alias: String, title: String, items: Vec<SearchResultItem>) -> Self {
        Self {
            server_alias,
            title,
            items,
            page_start: 0,
            page_size: 30,
            show_server_suffix: false,
        }
    }

    fn new_multi_server(title: String, items: Vec<SearchResultItem>) -> Self {
        Self {
            server_alias: "all".to_string(),
            title,
            items,
            page_start: 0,
            page_size: 30,
            show_server_suffix: true,
        }
    }

    fn visible_range(&self) -> std::ops::Range<usize> {
        let start = self.page_start.min(self.items.len());
        let end = start.saturating_add(self.page_size).min(self.items.len());
        start..end
    }

    fn page_count(&self) -> usize {
        if self.items.is_empty() {
            1
        } else {
            (self.items.len() + self.page_size - 1) / self.page_size
        }
    }

    fn current_page_number(&self) -> usize {
        if self.items.is_empty() {
            1
        } else {
            (self.page_start / self.page_size) + 1
        }
    }
}

#[derive(Clone)]
struct HelpState {
    title: String,
    lines: Vec<String>,
    page_start: usize,
    page_size: usize,
    topic_number: Option<usize>,
}

impl HelpState {
    fn new(title: String, lines: Vec<String>) -> Self {
        Self {
            title,
            lines,
            page_start: 0,
            page_size: 30,
            topic_number: None,
        }
    }

    fn new_topic(title: String, lines: Vec<String>, topic_number: usize) -> Self {
        Self {
            title,
            lines,
            page_start: 0,
            page_size: 30,
            topic_number: Some(topic_number),
        }
    }

    fn visible_range(&self) -> std::ops::Range<usize> {
        let start = self.page_start.min(self.lines.len());
        let end = start.saturating_add(self.page_size).min(self.lines.len());
        start..end
    }

    fn page_count(&self) -> usize {
        if self.lines.is_empty() {
            1
        } else {
            (self.lines.len() + self.page_size - 1) / self.page_size
        }
    }

    fn current_page_number(&self) -> usize {
        if self.lines.is_empty() {
            1
        } else {
            (self.page_start / self.page_size) + 1
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SelectionContext {
    Results,
    Queue,
    SavedQueues,
    Recent,
    Messages,
    Help,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RepeatMode {
    Off,
    One,
    All,
}

impl RepeatMode {
    fn label(self) -> &'static str {
        match self {
            RepeatMode::Off => "off",
            RepeatMode::One => "one",
            RepeatMode::All => "all",
        }
    }
}

#[derive(Clone)]
struct TerminalTheme {
    name: String,
    id: u8,
    fg: Color,
    dim: Color,
    panel: Color,
    accent: Color,
    secondary: Color,
    danger: Color,
    warning: Color,
    queue_palette: [Color; 4],
}

impl TerminalTheme {
    fn label(&self) -> String {
        if self.id == 0 {
            format!("custom - {}", self.name)
        } else {
            format!("{} - {}", self.id, self.name.to_uppercase())
        }
    }

    fn is_multicolour(&self) -> bool {
        self.name.starts_with("multi-") || self.queue_palette.iter().any(|colour| *colour != self.queue_palette[0])
    }

    fn block_style(&self) -> Style {
        Style::default().fg(self.fg).bg(self.panel)
    }

    fn title_style(&self) -> Style {
        Style::default().fg(self.accent).bg(self.panel).add_modifier(Modifier::BOLD)
    }
}

fn rgb(hex: u32) -> Color {
    Color::Rgb(
        ((hex >> 16) & 0xff) as u8,
        ((hex >> 8) & 0xff) as u8,
        (hex & 0xff) as u8,
    )
}

fn terminal_theme(name: &str) -> TerminalTheme {
    match normalize_style_token(name).as_str() {
        "mid" => TerminalTheme {
            name: "mid".to_string(),
            id: 2,
            fg: rgb(0x48d548),
            dim: rgb(0x48d548),
            panel: rgb(0x060806),
            accent: rgb(0x48d548),
            secondary: rgb(0xcc6030),
            danger: rgb(0xff936f),
            warning: rgb(0xffcf70),
            queue_palette: [rgb(0x48d548), rgb(0x48d548), rgb(0x48d548), rgb(0x48d548)],
        },
        "bright" => TerminalTheme {
            name: "bright".to_string(),
            id: 3,
            fg: rgb(0x00ff00),
            dim: rgb(0x00ff00),
            panel: rgb(0x000000),
            accent: rgb(0x00ff00),
            secondary: rgb(0xcc6600),
            danger: rgb(0xff9966),
            warning: rgb(0xffcc66),
            queue_palette: [rgb(0x00ff00), rgb(0x00ff00), rgb(0x00ff00), rgb(0x00ff00)],
        },
        "multi-soft" => TerminalTheme {
            name: "multi-soft".to_string(),
            id: 4,
            fg: rgb(0x8fb996),
            dim: rgb(0x8fb996),
            panel: rgb(0x0b0d0c),
            accent: rgb(0x8fb996),
            secondary: rgb(0xcc5a2a),
            danger: rgb(0xff8e8e),
            warning: rgb(0xffd27a),
            queue_palette: [rgb(0x8fb996), rgb(0xc86f38), rgb(0x73a7c6), rgb(0xc7b55a)],
        },
        "multi-mid" => TerminalTheme {
            name: "multi-mid".to_string(),
            id: 5,
            fg: rgb(0x48d548),
            dim: rgb(0x48d548),
            panel: rgb(0x060806),
            accent: rgb(0x48d548),
            secondary: rgb(0xcc6030),
            danger: rgb(0xff936f),
            warning: rgb(0xffcf70),
            queue_palette: [rgb(0x48d548), rgb(0xe07a33), rgb(0x4ea7ef), rgb(0xe0d14a)],
        },
        "multi-bright" => TerminalTheme {
            name: "multi-bright".to_string(),
            id: 6,
            fg: rgb(0x00ff00),
            dim: rgb(0x00ff00),
            panel: rgb(0x000000),
            accent: rgb(0x00ff00),
            secondary: rgb(0xcc6600),
            danger: rgb(0xff9966),
            warning: rgb(0xffcc66),
            queue_palette: [rgb(0x00ff00), rgb(0xff7f00), rgb(0x00a8ff), rgb(0xffff33)],
        },
        _ => TerminalTheme {
            name: "soft".to_string(),
            id: 1,
            fg: rgb(0x8fb996),
            dim: rgb(0x8fb996),
            panel: rgb(0x0b0d0c),
            accent: rgb(0x8fb996),
            secondary: rgb(0xcc5a2a),
            danger: rgb(0xff8e8e),
            warning: rgb(0xffd27a),
            queue_palette: [rgb(0x8fb996), rgb(0x8fb996), rgb(0x8fb996), rgb(0x8fb996)],
        },
    }
}

fn normalize_style_token(value: &str) -> String {
    value.trim().to_lowercase().trim_start_matches('-').to_string()
}

fn resolve_style_token(value: &str) -> Option<&'static str> {
    match normalize_style_token(value).as_str() {
        "1" | "soft" => Some("soft"),
        "2" | "mid" => Some("mid"),
        "3" | "bright" => Some("bright"),
        "4" | "multi-soft" | "multisoft" => Some("multi-soft"),
        "5" | "multi-mid" | "multimid" => Some("multi-mid"),
        "6" | "multi-bright" | "multibright" => Some("multi-bright"),
        _ => None,
    }
}

fn style_list_label() -> &'static str {
    "1 soft, 2 mid, 3 bright, 4 multi-soft, 5 multi-mid, 6 multi-bright"
}

#[derive(Clone, Debug, Deserialize)]
struct CustomStylesFile {
    #[serde(default)]
    styles: Vec<CustomStyleDefinition>,
}

#[derive(Clone, Debug, Deserialize)]
struct CustomStyleDefinition {
    name: String,
    #[serde(default)]
    base: Option<String>,
    #[serde(default, alias = "foreground")]
    fg: Option<String>,
    #[serde(default, alias = "primary")]
    accent: Option<String>,
    #[serde(default, alias = "background")]
    panel: Option<String>,
    #[serde(default)]
    dim: Option<String>,
    #[serde(default)]
    secondary: Option<String>,
    #[serde(default)]
    danger: Option<String>,
    #[serde(default)]
    warning: Option<String>,
    #[serde(default, alias = "queue_album_colours", alias = "queue_album_colors")]
    queue_palette: Option<Vec<String>>,
}

fn terminal_theme_from_custom(name: &str, custom_styles: &[CustomStyleDefinition]) -> TerminalTheme {
    if let Some(custom) = custom_style_by_name(custom_styles, name) {
        custom_theme(custom)
    } else {
        terminal_theme(name)
    }
}

fn custom_style_by_name<'a>(custom_styles: &'a [CustomStyleDefinition], name: &str) -> Option<&'a CustomStyleDefinition> {
    let normalized = normalize_custom_style_token(name);
    custom_styles
        .iter()
        .find(|style| normalize_style_token(&style.name) == normalized)
}

fn normalize_custom_style_token(value: &str) -> String {
    normalize_style_token(value)
        .trim_start_matches("custom:")
        .trim_start_matches("custom-")
        .to_string()
}

fn custom_theme(definition: &CustomStyleDefinition) -> TerminalTheme {
    let mut theme = definition
        .base
        .as_deref()
        .map(terminal_theme)
        .unwrap_or_else(|| terminal_theme("soft"));
    theme.id = 0;
    theme.name = definition.name.clone();

    if let Some(colour) = definition.fg.as_deref().and_then(parse_colour_token) {
        theme.fg = colour;
    }
    if let Some(colour) = definition.accent.as_deref().and_then(parse_colour_token) {
        theme.accent = colour;
    }
    if let Some(colour) = definition.panel.as_deref().and_then(parse_colour_token) {
        theme.panel = colour;
    }
    if let Some(colour) = definition.dim.as_deref().and_then(parse_colour_token) {
        theme.dim = colour;
    }
    if let Some(colour) = definition.secondary.as_deref().and_then(parse_colour_token) {
        theme.secondary = colour;
    }
    if let Some(colour) = definition.danger.as_deref().and_then(parse_colour_token) {
        theme.danger = colour;
    }
    if let Some(colour) = definition.warning.as_deref().and_then(parse_colour_token) {
        theme.warning = colour;
    }
    if let Some(palette) = &definition.queue_palette {
        let parsed: Vec<Color> = palette.iter().filter_map(|value| parse_colour_token(value)).collect();
        if !parsed.is_empty() {
            for idx in 0..theme.queue_palette.len() {
                theme.queue_palette[idx] = parsed[idx % parsed.len()];
            }
        }
    }

    theme
}

fn load_custom_styles_from_store(store: &ConfigStore) -> (Vec<CustomStyleDefinition>, Option<String>) {
    let path = store.custom_styles_path();
    if !path.exists() {
        return (Vec::new(), None);
    }
    match fs::read_to_string(&path) {
        Ok(text) => match toml::from_str::<CustomStylesFile>(&text) {
            Ok(file) => (file.styles, None),
            Err(error) => (Vec::new(), Some(format!("could not parse {}: {}", path.display(), error))),
        },
        Err(error) => (Vec::new(), Some(format!("could not read {}: {}", path.display(), error))),
    }
}

fn custom_styles_label(custom_styles: &[CustomStyleDefinition]) -> String {
    if custom_styles.is_empty() {
        "none loaded".to_string()
    } else {
        custom_styles
            .iter()
            .map(|style| style.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn parse_colour_token(value: &str) -> Option<Color> {
    let raw = value.trim();
    let normalized = raw.to_lowercase().replace('_', "-");
    match normalized.as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" | "purple" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        "gray" | "grey" => Some(Color::Gray),
        "dark-gray" | "dark-grey" => Some(Color::DarkGray),
        "light-red" => Some(Color::LightRed),
        "light-green" => Some(Color::LightGreen),
        "light-yellow" => Some(Color::LightYellow),
        "light-blue" => Some(Color::LightBlue),
        "light-magenta" | "light-purple" => Some(Color::LightMagenta),
        "light-cyan" => Some(Color::LightCyan),
        "orange" => Some(rgb(0xcc5a2a)),
        _ => parse_hex_colour(raw)
            .or_else(|| parse_rgb_function(raw))
            .or_else(|| parse_ansi_function(raw))
            .or_else(|| parse_cmyk_function(raw)),
    }
}

fn parse_hex_colour(value: &str) -> Option<Color> {
    let hex = value.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    u32::from_str_radix(hex, 16).ok().map(rgb)
}

fn parse_rgb_function(value: &str) -> Option<Color> {
    let inside = function_args(value, "rgb")?;
    let parts = parse_number_list(&inside)?;
    if parts.len() != 3 {
        return None;
    }
    Some(Color::Rgb(
        parts[0].round().clamp(0.0, 255.0) as u8,
        parts[1].round().clamp(0.0, 255.0) as u8,
        parts[2].round().clamp(0.0, 255.0) as u8,
    ))
}

fn parse_ansi_function(value: &str) -> Option<Color> {
    let inside = function_args(value, "ansi")?;
    inside.trim().parse::<u8>().ok().map(Color::Indexed)
}

fn parse_cmyk_function(value: &str) -> Option<Color> {
    let inside = function_args(value, "cmyk")?;
    let mut parts = parse_number_list(&inside)?;
    if parts.len() != 4 {
        return None;
    }
    if parts.iter().any(|v| *v > 1.0) {
        for part in &mut parts {
            *part /= 100.0;
        }
    }
    let c = parts[0].clamp(0.0, 1.0);
    let m = parts[1].clamp(0.0, 1.0);
    let y = parts[2].clamp(0.0, 1.0);
    let k = parts[3].clamp(0.0, 1.0);
    Some(Color::Rgb(
        (255.0 * (1.0 - c) * (1.0 - k)).round().clamp(0.0, 255.0) as u8,
        (255.0 * (1.0 - m) * (1.0 - k)).round().clamp(0.0, 255.0) as u8,
        (255.0 * (1.0 - y) * (1.0 - k)).round().clamp(0.0, 255.0) as u8,
    ))
}

fn function_args(value: &str, name: &str) -> Option<String> {
    let trimmed = value.trim();
    let lower = trimmed.to_lowercase();
    let prefix = format!("{}(", name);
    if !lower.starts_with(&prefix) || !trimmed.ends_with(')') {
        return None;
    }
    Some(trimmed[prefix.len()..trimmed.len() - 1].to_string())
}

fn parse_number_list(value: &str) -> Option<Vec<f64>> {
    value
        .split(',')
        .map(|part| part.trim().trim_end_matches('%').parse::<f64>().ok())
        .collect()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct SavedQueue {
    name: String,
    current_index: Option<usize>,
    tracks: Vec<QueueTrack>,
}

#[derive(Clone, Debug)]
struct SavedQueueListEntry {
    name: String,
    track_count: usize,
    current_index: Option<usize>,
    sources: Vec<String>,
    first_track_label: Option<String>,
    path: PathBuf,
    unreadable: bool,
}

#[derive(Clone, Debug)]
struct ServerPlaylistTarget {
    id: String,
    name: String,
    server_alias: String,
}

#[derive(Clone, Debug)]
struct SavedQueueListState {
    entries: Vec<SavedQueueListEntry>,
    page_start: usize,
    page_size: usize,
}

impl SavedQueueListState {
    fn new(entries: Vec<SavedQueueListEntry>) -> Self {
        Self {
            entries,
            page_start: 0,
            page_size: 30,
        }
    }

    fn visible_range(&self) -> std::ops::Range<usize> {
        let start = self.page_start.min(self.entries.len());
        let end = start.saturating_add(self.page_size).min(self.entries.len());
        start..end
    }

    fn page_count(&self) -> usize {
        if self.entries.is_empty() {
            1
        } else {
            (self.entries.len() + self.page_size - 1) / self.page_size
        }
    }

    fn current_page_number(&self) -> usize {
        if self.entries.is_empty() {
            1
        } else {
            (self.page_start / self.page_size) + 1
        }
    }
}

struct AppState {
    config: AppConfig,
    store: ConfigStore,
    input: String,
    input_cursor: usize,
    messages: Vec<String>,
    wizard: Option<ServerWizard>,
    history: Vec<String>,
    history_index: Option<usize>,
    history_draft: String,
    results: Option<ResultsState>,
    results_stack: Vec<ResultsState>,
    genre_cache: HashMap<String, Vec<SearchResultItem>>,
    playlist_cache: HashMap<String, Vec<SearchResultItem>>,
    artist_cache: HashMap<String, Vec<SearchResultItem>>,
    saved_queues: Option<SavedQueueListState>,
    help: Option<HelpState>,
    recent_tracks: Vec<QueueTrack>,
    recent_page_start: usize,
    recent_page_size: usize,
    queue: Vec<QueueTrack>,
    queue_playlist_name: Option<String>,
    current_queue_index: Option<usize>,
    queue_page_start: usize,
    queue_page_size: usize,
    selection_context: Option<SelectionContext>,
    last_non_queue_context: Option<SelectionContext>,
    paused_selection_pending: bool,
    user_pause_requested: bool,
    repeat_mode: RepeatMode,
    shuffle_enabled: bool,
    shuffle_order_enabled: bool,
    shuffle_original_order: Option<Vec<QueueTrack>>,
    shuffle_play_order: Vec<usize>,
    shuffle_play_position: Option<usize>,
    force_queue_view_next: bool,
    previous_volume_before_mute: Option<u8>,
    playback: Option<Arc<PlaybackEngine>>,
    playback_init_error: Option<String>,
    custom_styles: Vec<CustomStyleDefinition>,
    custom_styles_error: Option<String>,
    media_controls: Option<MediaIntegration>,
    media_controls_error: Option<String>,
    pending_search: Option<PendingSearch>,
    pending_queue_playback: Option<PendingQueuePlayback>,
    runtime_stderr_capture: Option<RuntimeStderrCapture>,
    quit_requested: bool,
}

impl AppState {
    fn new(config: AppConfig, store: ConfigStore) -> Self {
        let mut messages = Vec::new();
        messages.push(format!("DISC {} release candidate. Type 'help' or 'h' for the help index.", BUILD_LABEL));
        if store.session_path().exists() {
            messages.push("Last session is available. Use 'restore-session', 'rs', or 'session restore' to reload it.".to_string());
        }

        let (playback, playback_init_error) = match PlaybackEngine::new() {
            Ok(engine) => (Some(Arc::new(engine)), None),
            Err(error) => (None, Some(error.to_string())),
        };
        if let Some(error) = &playback_init_error {
            messages.push(format!("Playback disabled: {}", error));
        }

        let (custom_styles, custom_styles_error) = load_custom_styles_from_store(&store);
        if let Some(error) = &custom_styles_error {
            messages.push(format!("Custom styles unavailable: {}", error));
        } else if !custom_styles.is_empty() {
            messages.push(format!("Loaded {} custom style(s). Use style custom to list them.", custom_styles.len()));
        }

        let (media_controls, media_controls_error) = if config.media_keys_enabled {
            match MediaIntegration::new() {
                Ok(media) => {
                    messages.push("Media keys and now-playing metadata enabled.".to_string());
                    (Some(media), None)
                }
                Err(error) => {
                    messages.push(format!("Media keys unavailable: {}", error));
                    (None, Some(error.to_string()))
                }
            }
        } else {
            (None, None)
        };

        Self {
            config,
            store,
            input: String::new(),
            input_cursor: 0,
            messages,
            wizard: None,
            history: Vec::new(),
            history_index: None,
            history_draft: String::new(),
            results: None,
            results_stack: Vec::new(),
            genre_cache: HashMap::new(),
            playlist_cache: HashMap::new(),
            artist_cache: HashMap::new(),
            saved_queues: None,
            help: None,
            recent_tracks: Vec::new(),
            recent_page_start: 0,
            recent_page_size: 30,
            queue: Vec::new(),
            queue_playlist_name: None,
            current_queue_index: None,
            queue_page_start: 0,
            queue_page_size: 30,
            selection_context: None,
            last_non_queue_context: None,
            paused_selection_pending: false,
            user_pause_requested: false,
            repeat_mode: RepeatMode::Off,
            shuffle_enabled: false,
            shuffle_order_enabled: true,
            shuffle_original_order: None,
            shuffle_play_order: Vec::new(),
            shuffle_play_position: None,
            force_queue_view_next: false,
            previous_volume_before_mute: None,
            playback,
            playback_init_error,
            custom_styles,
            custom_styles_error,
            media_controls,
            media_controls_error,
            pending_search: None,
            pending_queue_playback: None,
            runtime_stderr_capture: None,
            quit_requested: false,
        }
    }

    fn push_message(&mut self, message: impl Into<String>) {
        self.messages.push(message.into());
        if self.messages.len() > 500 {
            let extra = self.messages.len() - 500;
            self.messages.drain(0..extra);
        }
    }

    fn reset_shuffle_play_order(&mut self) {
        self.shuffle_play_order.clear();
        self.shuffle_play_position = None;
    }

    fn queue_context_label(&self) -> Option<String> {
        self.queue_playlist_name
            .as_ref()
            .map(|label| label.trim())
            .filter(|label| !label.is_empty())
            .map(|label| label.to_string())
    }

    fn theme(&self) -> TerminalTheme {
        terminal_theme_from_custom(&self.config.theme, &self.custom_styles)
    }

    fn update_adaptive_page_sizes(&mut self, main_inner_height: usize) {
        let page_size = adaptive_page_size(main_inner_height);
        if let Some(results) = self.results.as_mut() {
            update_page_window(&mut results.page_start, &mut results.page_size, page_size, results.items.len());
        }
        if let Some(saved) = self.saved_queues.as_mut() {
            update_page_window(&mut saved.page_start, &mut saved.page_size, page_size, saved.entries.len());
        }
        if let Some(help) = self.help.as_mut() {
            update_page_window(&mut help.page_start, &mut help.page_size, page_size, help.lines.len());
        }
        update_page_window(&mut self.queue_page_start, &mut self.queue_page_size, page_size, self.queue.len());
        update_page_window(&mut self.recent_page_start, &mut self.recent_page_size, page_size, self.recent_tracks.len());
    }

    fn push_result_messages(&mut self, state: &ResultsState) {
        let total = state.items.len();
        if total == 0 {
            self.push_message(format!("{}: no results.", state.title));
            return;
        }

        if !self.config.advanced_status {
            return;
        }

        let range = state.visible_range();
        self.push_message(format!(
            "{} | {} | showing {}-{} of {} | page {}/{}:",
            state.title,
            self.result_kind_summary(state),
            range.start + 1,
            range.end,
            total,
            state.current_page_number(),
            state.page_count()
        ));

        for (visible_idx, item) in state.items[range.clone()].iter().enumerate() {
            let absolute_number = range.start + visible_idx + 1;
            self.push_message(format!(
                "{}. {}",
                absolute_number,
                self.result_item_label_for_state(item, state)
            ));
        }

        self.push_message(self.result_action_hint(state));
        if state.page_count() > 1 {
            self.push_message("Numbers are absolute across all results; ranges such as 'add 1-5 40 79' can refer to different pages.".to_string());
        }
    }

    fn push_saved_queue_messages(&mut self, state: &SavedQueueListState) {
        let total = state.entries.len();
        if total == 0 {
            self.push_message("No saved queues.".to_string());
            return;
        }

        let range = state.visible_range();
        if !self.config.advanced_status {
            return;
        }

        self.push_message(format!(
            "Saved queues | showing {}-{} of {} | page {}/{}:",
            range.start + 1,
            range.end,
            total,
            state.current_page_number(),
            state.page_count()
        ));

        for (visible_idx, entry) in state.entries[range.clone()].iter().enumerate() {
            let absolute_number = range.start + visible_idx + 1;
            let suffix = self.saved_queue_entry_suffix(entry);
            self.push_message(format!("{}. {} ({})", absolute_number, entry.name, suffix));
        }

        self.push_message("Actions: <n>/lq <n> loads and plays | a<n>/add <n> appends | dq <n> deletes | rq <n> <new-name> renames | [ / ] page.".to_string());
        if state.page_count() > 1 {
            self.push_message("Saved queue numbers are absolute across all pages.".to_string());
        }
    }

    fn saved_queue_entry_suffix(&self, entry: &SavedQueueListEntry) -> String {
        if entry.unreadable {
            return "unreadable".to_string();
        }

        let mut parts = vec![format!("{} track(s)", entry.track_count)];
        if let Some(current) = entry.current_index.map(|idx| idx + 1) {
            parts.push(format!("saved position {}", current));
        }
        if !entry.sources.is_empty() {
            parts.push(format!("server(s): {}", entry.sources.join(", ")));
        }
        if let Some(label) = &entry.first_track_label {
            parts.push(format!("starts: {}", label));
        }
        parts.join(", ")
    }

    fn set_saved_queues(&mut self, state: SavedQueueListState) {
        self.push_saved_queue_messages(&state);
        self.saved_queues = Some(state);
        self.selection_context = Some(SelectionContext::SavedQueues);
    }

    fn push_current_saved_queue_page(&mut self) {
        if let Some(state) = self.saved_queues.clone() {
            self.push_saved_queue_messages(&state);
            self.selection_context = Some(SelectionContext::SavedQueues);
        } else {
            self.push_message("No saved queue list. Use 'queues' first.".to_string());
        }
    }

    fn resolve_saved_queue_index(&self, index: usize) -> Option<SavedQueueListEntry> {
        let state = self.saved_queues.as_ref()?;
        state.entries.get(index).cloned()
    }

    fn set_results(&mut self, next: ResultsState) {
        if let Some(current) = self.results.take() {
            self.results_stack.push(current);
        }
        self.push_result_messages(&next);
        self.results = Some(next);
        self.selection_context = Some(SelectionContext::Results);
    }

    fn replace_results_without_push(&mut self, next: ResultsState) {
        self.push_result_messages(&next);
        self.results = Some(next);
        self.selection_context = Some(SelectionContext::Results);
    }

    fn pop_results(&mut self) -> Option<String> {
        match self.results_stack.pop() {
            Some(previous) => {
                let title = previous.title.clone();
                self.replace_results_without_push(previous);
                Some(title)
            }
            None => None,
        }
    }

    fn set_queue(&mut self, tracks: Vec<QueueTrack>) {
        self.queue = tracks;
        self.queue_playlist_name = None;
        self.shuffle_original_order = None;
        self.reset_shuffle_play_order();
        self.current_queue_index = if self.queue.is_empty() { None } else { Some(0) };
        self.queue_page_start = 0;
        self.paused_selection_pending = false;
        if self.queue.is_empty() {
            self.selection_context = None;
        }
        if self.queue.is_empty() {
            self.push_message("Queue cleared.");
        } else {
            if self.shuffle_enabled && self.shuffle_order_enabled {
                let _ = stable_shuffle_queue_order(self, false);
            }
            let current_index = self.current_queue_index.unwrap_or(0).min(self.queue.len().saturating_sub(1));
            let current_label = self.track_label(&self.queue[current_index]);
            self.push_message(format!(
                "Queue replaced with {} track(s). Current: {}",
                self.queue.len(),
                current_label
            ));
        }
        queue_playback_plan_changed(self);
    }

    fn append_queue(&mut self, tracks: Vec<QueueTrack>) {
        if tracks.is_empty() {
            self.push_message("Nothing to add to queue.");
            return;
        }
        let count = tracks.len();
        if !self.queue.is_empty() {
            self.queue_playlist_name = None;
        }
        if let Some(original) = self.shuffle_original_order.as_mut() {
            original.extend(tracks.iter().cloned());
        }
        self.reset_shuffle_play_order();
        self.queue.extend(tracks);
        if self.current_queue_index.is_none() && !self.queue.is_empty() {
            self.current_queue_index = Some(0);
        }
        if self.shuffle_enabled && self.shuffle_order_enabled {
            let _ = stable_shuffle_queue_order(self, false);
        }
        self.push_message(format!(
            "Added {} track(s) to queue. Queue length: {}",
            count,
            self.queue.len()
        ));
        queue_playback_plan_changed(self);
    }

    fn queue_visible_range(&self) -> std::ops::Range<usize> {
        let start = self.queue_page_start.min(self.queue.len());
        let end = start.saturating_add(self.queue_page_size).min(self.queue.len());
        start..end
    }

    fn queue_page_count(&self) -> usize {
        if self.queue.is_empty() {
            1
        } else {
            (self.queue.len() + self.queue_page_size - 1) / self.queue_page_size
        }
    }

    fn queue_current_page_number(&self) -> usize {
        if self.queue.is_empty() {
            1
        } else {
            (self.queue_page_start / self.queue_page_size) + 1
        }
    }

    fn ensure_queue_index_visible(&mut self, index: usize) {
        if self.queue.is_empty() {
            self.queue_page_start = 0;
            return;
        }
        if index < self.queue_page_start || index >= self.queue_page_start.saturating_add(self.queue_page_size) {
            self.queue_page_start = (index / self.queue_page_size) * self.queue_page_size;
        }
    }

    fn enter_queue_context(&mut self) {
        match self.selection_context {
            Some(SelectionContext::Results) | Some(SelectionContext::SavedQueues) | Some(SelectionContext::Recent) => {
                self.last_non_queue_context = self.selection_context;
            }
            _ => {}
        }
        self.selection_context = Some(SelectionContext::Queue);
    }

    fn show_queue_messages(&mut self) {
        self.enter_queue_context();
        if self.queue.is_empty() {
            self.queue_page_start = 0;
            self.push_message("Queue is empty.");
            return;
        }
        if !self.config.advanced_status {
            return;
        }
        let range = self.queue_visible_range();
        self.push_message(format!(
            "Queue | showing {}-{} of {} | page {}/{}:",
            range.start + 1,
            range.end,
            self.queue.len(),
            self.queue_current_page_number(),
            self.queue_page_count()
        ));
        let lines: Vec<String> = self
            .queue[range.clone()]
            .iter()
            .enumerate()
            .map(|(visible_idx, track)| {
                let idx = range.start + visible_idx;
                let marker = if Some(idx) == self.current_queue_index { ">" } else { " " };
                format!("{} {}. {}", marker, idx + 1, self.track_label(track))
            })
            .collect();
        for line in lines {
            self.push_message(line);
        }
        self.push_message("Actions: <n> selects queue item | PageUp/PageDown prev/next track | Left/Right prev/next only when command input is empty | remove/r<n...> removes | move <from> <to> reorders | shuffle/sh toggles | so stable shuffle order | dedupe/clear-played/clear-upcoming clean queue.".to_string());
        if self.queue_page_count() > 1 {
            self.push_message("Use '[' and ']' to page the queue. Queue numbers are absolute across all pages.".to_string());
        }
    }

    fn recent_visible_range(&self) -> std::ops::Range<usize> {
        let start = self.recent_page_start.min(self.recent_tracks.len());
        let end = start.saturating_add(self.recent_page_size).min(self.recent_tracks.len());
        start..end
    }

    fn recent_page_count(&self) -> usize {
        if self.recent_tracks.is_empty() {
            1
        } else {
            (self.recent_tracks.len() + self.recent_page_size - 1) / self.recent_page_size
        }
    }

    fn recent_current_page_number(&self) -> usize {
        if self.recent_tracks.is_empty() {
            1
        } else {
            (self.recent_page_start / self.recent_page_size) + 1
        }
    }

    fn record_recent_track(&mut self, track: &QueueTrack) {
        let key = queue_track_key(track);
        self.recent_tracks.retain(|existing| queue_track_key(existing) != key);
        self.recent_tracks.insert(0, track.clone());
        if self.recent_tracks.len() > 200 {
            self.recent_tracks.truncate(200);
        }
        self.recent_page_start = 0;
    }

    fn show_recent_messages(&mut self) {
        self.selection_context = Some(SelectionContext::Recent);
        if self.recent_tracks.is_empty() {
            self.recent_page_start = 0;
            self.push_message("No playback-history tracks in this session.".to_string());
            return;
        }

        if !self.config.advanced_status {
            return;
        }

        let range = self.recent_visible_range();
        self.push_message(format!(
            "Playback history | showing {}-{} of {} | page {}/{}:",
            range.start + 1,
            range.end,
            self.recent_tracks.len(),
            self.recent_current_page_number(),
            self.recent_page_count()
        ));

        let lines: Vec<String> = self
            .recent_tracks[range.clone()]
            .iter()
            .enumerate()
            .map(|(visible_idx, track)| {
                let idx = range.start + visible_idx;
                format!("{}. {}", idx + 1, self.track_label(track))
            })
            .collect();
        for line in lines {
            self.push_message(line);
        }

        self.push_message("Actions: <n> replaces queue with playback-history track and plays | a<n...>/add <n...> appends | [ / ] page | history clear clears this session history.".to_string());
        if self.recent_page_count() > 1 {
            self.push_message("Playback-history numbers are absolute across all pages.".to_string());
        }
    }

    fn clear_recent_tracks(&mut self) {
        self.recent_tracks.clear();
        self.recent_page_start = 0;
        if matches!(self.selection_context, Some(SelectionContext::Recent)) {
            self.selection_context = None;
        }
        self.push_message("Playback history cleared for this session.".to_string());
    }

    fn clear_queue(&mut self) {
        self.queue.clear();
        self.queue_playlist_name = None;
        self.shuffle_original_order = None;
        self.current_queue_index = None;
        self.queue_page_start = 0;
        self.paused_selection_pending = false;
        self.user_pause_requested = false;
        self.pending_queue_playback = None;
        self.selection_context = None;
        if let Some(engine) = &self.playback {
            engine.stop();
        }
        self.push_message("Queue cleared.");
    }

    fn input_cursor_chars(&self) -> usize {
        self.input[..self.input_cursor.min(self.input.len())].chars().count()
    }

    fn input_prefix_display_width(&self) -> usize {
        let cursor = self.input_cursor.min(self.input.len());
        UnicodeWidthStr::width(&self.input[..cursor])
    }

    fn set_input_value(&mut self, value: String) {
        self.input = value;
        self.input_cursor = self.input.len();
    }

    fn clear_input(&mut self) {
        self.input.clear();
        self.input_cursor = 0;
    }

    fn input_insert_char(&mut self, ch: char) {
        self.input_cursor = self.input_cursor.min(self.input.len());
        while self.input_cursor > 0 && !self.input.is_char_boundary(self.input_cursor) {
            self.input_cursor -= 1;
        }
        self.input.insert(self.input_cursor, ch);
        self.input_cursor += ch.len_utf8();
    }

    fn input_backspace(&mut self) {
        if self.input_cursor == 0 || self.input.is_empty() {
            return;
        }
        self.input_cursor = self.input_cursor.min(self.input.len());
        while self.input_cursor > 0 && !self.input.is_char_boundary(self.input_cursor) {
            self.input_cursor -= 1;
        }
        let previous = self.input[..self.input_cursor]
            .char_indices()
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
        self.input.drain(previous..self.input_cursor);
        self.input_cursor = previous;
    }

    fn input_cursor_left(&mut self) {
        if self.input_cursor == 0 {
            return;
        }
        self.input_cursor = self.input_cursor.min(self.input.len());
        while self.input_cursor > 0 && !self.input.is_char_boundary(self.input_cursor) {
            self.input_cursor -= 1;
        }
        self.input_cursor = self.input[..self.input_cursor]
            .char_indices()
            .last()
            .map(|(idx, _)| idx)
            .unwrap_or(0);
    }

    fn input_cursor_right(&mut self) {
        if self.input_cursor >= self.input.len() {
            self.input_cursor = self.input.len();
            return;
        }
        self.input_cursor = self.input_cursor.min(self.input.len());
        while self.input_cursor < self.input.len() && !self.input.is_char_boundary(self.input_cursor) {
            self.input_cursor += 1;
        }
        if let Some(ch) = self.input[self.input_cursor..].chars().next() {
            self.input_cursor += ch.len_utf8();
        } else {
            self.input_cursor = self.input.len();
        }
    }

    fn input_cursor_home(&mut self) {
        self.input_cursor = 0;
    }

    fn input_cursor_end(&mut self) {
        self.input_cursor = self.input.len();
    }

    fn commit_input_to_history(&mut self, command: &str) {
        let trimmed = command.trim();
        self.history_index = None;
        self.history_draft.clear();
        if trimmed.is_empty() {
            return;
        }
        if self.history.last().map(|entry| entry == trimmed).unwrap_or(false) {
            return;
        }
        self.history.push(trimmed.to_string());
        if self.history.len() > 200 {
            let extra = self.history.len() - 200;
            self.history.drain(0..extra);
        }
    }

    fn history_up(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {
                self.history_draft = self.input.clone();
                let idx = self.history.len() - 1;
                self.history_index = Some(idx);
                self.set_input_value(self.history[idx].clone());
            }
            Some(0) => {}
            Some(idx) => {
                let next_idx = idx.saturating_sub(1);
                self.history_index = Some(next_idx);
                self.set_input_value(self.history[next_idx].clone());
            }
        }
    }

    fn history_down(&mut self) {
        if self.history.is_empty() {
            return;
        }
        match self.history_index {
            None => {}
            Some(idx) if idx + 1 >= self.history.len() => {
                self.history_index = None;
                self.set_input_value(self.history_draft.clone());
                self.history_draft.clear();
            }
            Some(idx) => {
                let next_idx = idx + 1;
                self.history_index = Some(next_idx);
                self.set_input_value(self.history[next_idx].clone());
            }
        }
    }

    fn is_primary_alias(&self, alias: &str) -> bool {
        self.config
            .primary_server()
            .map(|server| server.alias.eq_ignore_ascii_case(alias))
            .unwrap_or(false)
    }

    fn track_label(&self, track: &QueueTrack) -> String {
        let base = format!("{} — {} • {}", track.title, track.artist, track.album);
        if self.is_primary_alias(&track.server_alias) {
            base
        } else {
            format!("{} [{}]", base, track.server_alias)
        }
    }

    fn playback_track_label(&self, track: &PlaybackTrack) -> String {
        let base = format!("{} — {} • {}", track.title, track.artist, track.album);
        if self.is_primary_alias(&track.server_alias) {
            base
        } else {
            format!("{} [{}]", base, track.server_alias)
        }
    }

    fn result_item_label(&self, item: &SearchResultItem) -> String {
        let show_server_suffix = self
            .results
            .as_ref()
            .map(|state| state.show_server_suffix)
            .unwrap_or(false);
        self.result_item_label_with_server_suffix(item, show_server_suffix)
    }

    fn result_item_label_for_state(&self, item: &SearchResultItem, state: &ResultsState) -> String {
        self.result_item_label_with_server_suffix(item, state.show_server_suffix)
    }

    fn result_item_label_with_server_suffix(&self, item: &SearchResultItem, show_server_suffix: bool) -> String {
        let subtitle = cleaned_result_subtitle(&item.subtitle, &item.server_alias);
        let mut label = if subtitle.is_empty() {
            item.title.clone()
        } else {
            format!("{} — {}", item.title, subtitle)
        };
        if show_server_suffix {
            label = format!("{} [{}]", label, item.server_alias);
        }
        label
    }

    fn resolve_result_selection(&self, index: usize) -> Option<SearchResultItem> {
        let results = self.results.as_ref()?;
        results.items.get(index).cloned()
    }

    fn resolve_result_index(&self, index: usize) -> Option<usize> {
        let results = self.results.as_ref()?;
        if index < results.items.len() {
            Some(index)
        } else {
            None
        }
    }

    fn result_count(&self) -> usize {
        self.results.as_ref().map(|results| results.items.len()).unwrap_or(0)
    }

    fn result_kind_summary(&self, state: &ResultsState) -> String {
        let mut tracks = 0usize;
        let mut albums = 0usize;
        let mut artists = 0usize;
        let mut genres = 0usize;
        let mut playlists = 0usize;
        let mut sections = 0usize;
        for item in &state.items {
            match item.kind {
                ResultKind::Track => tracks += 1,
                ResultKind::Album => albums += 1,
                ResultKind::Artist => artists += 1,
                ResultKind::Genre => genres += 1,
                ResultKind::Playlist => playlists += 1,
                ResultKind::Section => sections += 1,
            }
        }

        let mut parts = Vec::new();
        if tracks > 0 { parts.push(format!("{} track(s)", tracks)); }
        if albums > 0 { parts.push(format!("{} album(s)", albums)); }
        if artists > 0 { parts.push(format!("{} artist(s)", artists)); }
        if genres > 0 { parts.push(format!("{} genre(s)", genres)); }
        if playlists > 0 { parts.push(format!("{} playlist(s)", playlists)); }
        if sections > 0 { parts.push(format!("{} section(s)", sections)); }
        if parts.is_empty() {
            "empty".to_string()
        } else {
            parts.join(", ")
        }
    }

    fn result_action_hint(&self, state: &ResultsState) -> String {
        let has_tracks = state.items.iter().any(|item| matches!(item.kind, ResultKind::Track));
        let has_album_or_playlist = state.items.iter().any(|item| matches!(item.kind, ResultKind::Album | ResultKind::Playlist));
        let has_artist_or_genre = state.items.iter().any(|item| matches!(item.kind, ResultKind::Artist | ResultKind::Genre));
        let has_star_supported = state.items.iter().any(|item| matches!(item.kind, ResultKind::Track | ResultKind::Album | ResultKind::Artist));

        let mut hints = Vec::new();
        if has_tracks {
            hints.push("<n> plays track".to_string());
        }
        if has_album_or_playlist {
            hints.push("<n> plays whole album/playlist".to_string());
            hints.push("x<n> explores tracks".to_string());
        }
        if has_artist_or_genre {
            hints.push("<n> explores".to_string());
        }
        if !state.items.is_empty() {
            hints.push("a<n...>/add <n...> appends".to_string());
        }
        if has_star_supported {
            hints.push("star <n...>/unstar <n...> updates favourites".to_string());
        }
        if state.page_count() > 1 {
            hints.push("[ / ] page".to_string());
            hints.push("page <n> jumps".to_string());
        }

        if hints.is_empty() {
            "Actions: no selectable results.".to_string()
        } else {
            format!("Actions: {}.", hints.join(" | "))
        }
    }

    fn resolve_result_index_for_item(&self, needle: &SearchResultItem) -> Option<usize> {
        self.results.as_ref()?.items.iter().position(|item| {
            item.kind == needle.kind
                && item.server_alias.eq_ignore_ascii_case(&needle.server_alias)
                && item.title == needle.title
                && item.target_id == needle.target_id
        })
    }

    fn push_current_result_page(&mut self) {
        if let Some(state) = self.results.clone() {
            self.push_result_messages(&state);
            self.selection_context = Some(SelectionContext::Results);
        } else {
            self.push_message("No current results.".to_string());
        }
    }

    fn persist_config(&self) -> Result<()> {
        self.store.save(&self.config)
    }

    fn target_client(&self, explicit_target: Option<&str>) -> Result<SubsonicClient> {
        let server = match explicit_target {
            Some(target) => self.config.find_server(target).cloned(),
            None => self.config.primary_server().cloned(),
        }
        .ok_or_else(|| anyhow::anyhow!("No matching server configured."))?;
        Ok(SubsonicClient::new(server))
    }
}

#[derive(Clone)]
enum WizardKind {
    Add,
    Edit { target: String },
}

#[derive(Clone)]
enum WizardStep {
    Name,
    Alias,
    BaseUrl,
    Username,
    Password,
    Primary,
}

#[derive(Clone)]
struct ServerWizard {
    kind: WizardKind,
    step: WizardStep,
    draft: ServerConfig,
    make_primary: bool,
}

impl ServerWizard {
    fn new_add() -> Self {
        Self {
            kind: WizardKind::Add,
            step: WizardStep::Name,
            draft: ServerConfig {
                name: String::new(),
                alias: String::new(),
                base_url: String::new(),
                username: String::new(),
                password: String::new(),
                search_timeout_seconds: crate::config::model::default_search_timeout_seconds(),
            },
            make_primary: false,
        }
    }

    fn new_edit(target: String, existing: &ServerConfig, is_primary: bool) -> Self {
        Self {
            kind: WizardKind::Edit { target },
            step: WizardStep::Name,
            draft: existing.clone(),
            make_primary: is_primary,
        }
    }

    fn step_label(&self) -> &'static str {
        match self.step {
            WizardStep::Name => "server name",
            WizardStep::Alias => "server alias",
            WizardStep::BaseUrl => "server URL",
            WizardStep::Username => "username",
            WizardStep::Password => "password",
            WizardStep::Primary => "make primary (y/n)",
        }
    }

    fn advance(&mut self) -> bool {
        self.step = match self.step {
            WizardStep::Name => WizardStep::Alias,
            WizardStep::Alias => WizardStep::BaseUrl,
            WizardStep::BaseUrl => WizardStep::Username,
            WizardStep::Username => WizardStep::Password,
            WizardStep::Password => WizardStep::Primary,
            WizardStep::Primary => return false,
        };
        true
    }
}

async fn handle_enter(app: &mut AppState, command: &str) -> Result<()> {
    if app.wizard.is_some() {
        return handle_wizard_input(app, command).await;
    }
    let result = handle_command(app, command).await;
    app.force_queue_view_next = false;
    result
}


async fn discover_server_base_url(draft: &ServerConfig) -> Result<Option<(String, Option<String>)>> {
    let candidates = candidate_server_urls(draft.base_url.as_str());
    for (url, note) in candidates {
        let mut trial = draft.clone();
        trial.base_url = url.clone();
        trial.search_timeout_seconds = 5;
        let client = SubsonicClient::new(trial);
        if client.ping().await.is_ok() {
            return Ok(Some((url, note)));
        }
    }
    Ok(None)
}

fn candidate_server_urls(input: &str) -> Vec<(String, Option<String>)> {
    let raw = input.trim().trim_end_matches('/');
    if raw.is_empty() {
        return Vec::new();
    }
    let has_scheme = raw.starts_with("http://") || raw.starts_with("https://");
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    if has_scheme {
        push_candidate_server_url(&mut out, &mut seen, raw.to_string(), None);
        if raw.starts_with("http://") && !host_part_has_port(raw.trim_start_matches("http://")) {
            push_candidate_server_url(&mut out, &mut seen, url_with_default_port(raw, 4040), Some("added common Subsonic http port 4040".to_string()));
        }
        if raw.starts_with("https://") && !host_part_has_port(raw.trim_start_matches("https://")) {
            push_candidate_server_url(&mut out, &mut seen, url_with_default_port(raw, 4443), Some("added common Subsonic https port 4443".to_string()));
        }
    } else if host_part_has_port(raw) {
        push_candidate_server_url(&mut out, &mut seen, format!("http://{}", raw), Some("added http://".to_string()));
        push_candidate_server_url(&mut out, &mut seen, format!("https://{}", raw), Some("added https://".to_string()));
    } else {
        push_candidate_server_url(&mut out, &mut seen, url_with_default_port(&format!("http://{}", raw), 4040), Some("added http:// and common Subsonic port 4040".to_string()));
        push_candidate_server_url(&mut out, &mut seen, url_with_default_port(&format!("https://{}", raw), 4443), Some("added https:// and common Subsonic port 4443".to_string()));
        push_candidate_server_url(&mut out, &mut seen, format!("http://{}", raw), Some("added http://".to_string()));
        push_candidate_server_url(&mut out, &mut seen, format!("https://{}", raw), Some("added https://".to_string()));
    }
    out
}

fn push_candidate_server_url(out: &mut Vec<(String, Option<String>)>, seen: &mut HashSet<String>, url: String, note: Option<String>) {
    if seen.insert(url.to_lowercase()) {
        out.push((url, note));
    }
}

fn host_part_has_port(host_and_path: &str) -> bool {
    let host = host_and_path.split('/').next().unwrap_or(host_and_path);
    host.rsplit(':')
        .next()
        .and_then(|value| value.parse::<u16>().ok())
        .is_some()
}

fn url_with_default_port(url: &str, port: u16) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return format!("{}:{}", url.trim_end_matches('/'), port);
    };
    let (host, path) = match rest.split_once('/') {
        Some((host, path)) => (host, format!("/{}", path)),
        None => (rest, String::new()),
    };
    format!("{}://{}:{}{}", scheme, host, port, path)
}

async fn handle_wizard_input(app: &mut AppState, value: &str) -> Result<()> {
    let mut wizard = app.wizard.clone().expect("wizard present");
    let trimmed = value.trim();

    match wizard.step {
        WizardStep::Name => {
            if !trimmed.is_empty() {
                wizard.draft.name = trimmed.to_string();
            }
        }
        WizardStep::Alias => {
            if !trimmed.is_empty() {
                let alias = trimmed.to_lowercase();
                if is_reserved_server_alias(&alias) {
                    app.push_message(format!(
                        "Alias '{}' is reserved for commands. Reserved aliases: {}.",
                        alias,
                        reserved_server_aliases_label()
                    ));
                    app.push_message(wizard_prompt(&wizard));
                    app.wizard = Some(wizard);
                    return Ok(());
                }
                wizard.draft.alias = alias;
            }
        }
        WizardStep::BaseUrl => {
            if !trimmed.is_empty() {
                wizard.draft.base_url = trimmed.to_string();
            }
        }
        WizardStep::Username => {
            if !trimmed.is_empty() {
                wizard.draft.username = trimmed.to_string();
            }
        }
        WizardStep::Password => {
            if !trimmed.is_empty() {
                wizard.draft.password = trimmed.to_string();
            }
        }
        WizardStep::Primary => {
            if !trimmed.is_empty() {
                let lower = trimmed.to_lowercase();
                wizard.make_primary = matches!(lower.as_str(), "y" | "yes" | "1" | "true");
            }
        }
    }

    if wizard.advance() {
        let prompt = wizard_prompt(&wizard);
        app.wizard = Some(wizard);
        app.push_message(prompt);
        return Ok(());
    }

    let mut draft = wizard.draft.clone();
    let make_primary = wizard.make_primary;
    match discover_server_base_url(&draft).await {
        Ok(Some((resolved_url, note))) => {
            if draft.base_url.trim() != resolved_url {
                app.push_message(format!("Server URL resolved to {}{}", resolved_url, note.map(|n| format!(" ({})", n)).unwrap_or_default()));
                draft.base_url = resolved_url;
            }
        }
        Ok(None) => {
            app.push_message("Warning: DISC could not verify this server URL. It will be saved as entered; use ping after setup to test it.".to_string());
        }
        Err(error) => {
            app.push_message(format!("Warning: server URL discovery failed: {}. Saving the URL as entered.", error));
        }
    }
    match wizard.kind {
        WizardKind::Add => {
            let alias = draft.alias.clone();
            app.config.add_or_update_server(draft);
            if make_primary {
                app.config.set_primary_by_target(&alias)?;
            }
            app.persist_config()?;
            app.push_message(format!("Saved server. Primary: {}", app.config.primary_display_name()));
        }
        WizardKind::Edit { target } => {
            let alias = draft.alias.clone();
            app.config.update_server_by_target(&target, draft)?;
            if make_primary {
                app.config.set_primary_by_target(&alias)?;
            }
            app.persist_config()?;
            app.push_message(format!("Updated server. Primary: {}", app.config.primary_display_name()));
        }
    }
    app.wizard = None;
    Ok(())
}

fn wizard_prompt(wizard: &ServerWizard) -> String {
    match wizard.step {
        WizardStep::Name => "Enter server name:".to_string(),
        WizardStep::Alias => {
            let default = if wizard.draft.alias.trim().is_empty() {
                String::new()
            } else {
                format!(" [{}]", wizard.draft.alias)
            };
            format!(
                "Enter server alias{}: short nickname for this server, used to make server-specific commands more convenient, e.g. 'sub rec' or 'hel rnd 50'.",
                default
            )
        },
        WizardStep::BaseUrl => format!("Enter server URL [{}]: you can enter a full URL such as http://host:4040, or just a host name and DISC will try common Subsonic defaults.", wizard.draft.base_url),
        WizardStep::Username => format!("Enter username [{}]:", wizard.draft.username),
        WizardStep::Password => {
            if wizard.draft.password.is_empty() {
                "Enter password:".to_string()
            } else {
                "Enter password [leave blank to keep current]:".to_string()
            }
        }
        WizardStep::Primary => {
            let default_hint = if wizard.make_primary { "y" } else { "n" };
            format!("Make this the primary server? [{}]:", default_hint)
        }
    }
}

async fn handle_command(app: &mut AppState, command: &str) -> Result<()> {
    let command = command.trim();
    if command.is_empty() {
        return Ok(());
    }

    if command.eq_ignore_ascii_case("kill") || command.eq_ignore_ascii_case("k") || command.eq_ignore_ascii_case("cancel") {
        handle_kill_command(app);
        return Ok(());
    }

    let expanded_command = expand_compact_single_letter_command(command);
    let (command_without_queue_suffix, force_queue_view) = strip_trailing_queue_view_suffix(expanded_command.as_str());
    app.force_queue_view_next = force_queue_view;
    let command_normalized = if force_queue_view {
        expand_compact_single_letter_command(command_without_queue_suffix)
    } else {
        expanded_command
    };
    let command = command_normalized.as_str();

    if let Ok(selection_number) = command.parse::<usize>() {
        if selection_number == 0 {
            return Ok(());
        }
        if app.selection_context == Some(SelectionContext::Help) {
            show_numbered_help_page(app, command)?;
            return Ok(());
        }
        if app.selection_context.is_none() && show_home_help_topic(app, selection_number) {
            return Ok(());
        }
        return handle_numeric_selection(app, selection_number - 1).await;
    }

    if looks_like_loose_multi_add_args(command) {
        if handle_loose_multi_add_command(app, command).await? {
            return Ok(());
        }
    }

    if command.eq_ignore_ascii_case("help") || command.eq_ignore_ascii_case("h") {
        show_help(app, None);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "help ").or_else(|| strip_command_prefix(command, "h ")) {
        show_help(app, Some(rest.trim()));
        return Ok(());
    }

    if command.eq_ignore_ascii_case("home") {
        app.selection_context = None;
        app.push_message("Showing home panel.".to_string());
        return Ok(());
    }

    if command.eq_ignore_ascii_case("view") || command.eq_ignore_ascii_case("views") {
        show_view_status(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "view ")
        .or_else(|| strip_command_prefix(command, "show "))
        .or_else(|| strip_command_prefix(command, "panel "))
        .or_else(|| strip_command_prefix(command, "focus "))
    {
        handle_view_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("messages")
        || command.eq_ignore_ascii_case("message")
        || command.eq_ignore_ascii_case("msg")
    {
        handle_messages_visibility_command(app, "toggle")?;
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "messages ")
        .or_else(|| strip_command_prefix(command, "message "))
        .or_else(|| strip_command_prefix(command, "msg "))
    {
        let rest = rest.trim();
        if rest.eq_ignore_ascii_case("log") || rest.eq_ignore_ascii_case("view") || rest.eq_ignore_ascii_case("show") {
            app.selection_context = Some(SelectionContext::Messages);
            app.push_message("Showing message log in the main page. Use cls to clear it or view home to return.".to_string());
        } else {
            handle_messages_visibility_command(app, rest)?;
        }
        return Ok(());
    }

    if command.eq_ignore_ascii_case("message-log") || command.eq_ignore_ascii_case("log") {
        app.selection_context = Some(SelectionContext::Messages);
        app.push_message("Showing message log in the main page. Use cls to clear it or view home to return.".to_string());
        return Ok(());
    }

    if command.eq_ignore_ascii_case("verbose toggle") || command.eq_ignore_ascii_case("vt") {
        handle_advanced_status_command(app, "toggle")?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("verbose") {
        handle_advanced_status_command(app, "toggle")?;
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "verbose ") {
        handle_advanced_status_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("advanced")
        || command.eq_ignore_ascii_case("advanced-status")
        || command.eq_ignore_ascii_case("debug")
        || command.eq_ignore_ascii_case("debug-ui")
    {
        show_advanced_status(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "advanced ")
        .or_else(|| strip_command_prefix(command, "advanced-status "))
        .or_else(|| strip_command_prefix(command, "debug "))
        .or_else(|| strip_command_prefix(command, "debug-ui "))
    {
        handle_advanced_status_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("sty")
        || command.eq_ignore_ascii_case("style")
        || command.eq_ignore_ascii_case("theme")
    {
        show_style_status(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "sty ")
        .or_else(|| strip_command_prefix(command, "style "))
        .or_else(|| strip_command_prefix(command, "theme "))
        .or_else(|| strip_command_prefix(command, "config sty "))
        .or_else(|| strip_command_prefix(command, "conf sty "))
        .or_else(|| strip_command_prefix(command, "ideaconfig sty "))
    {
        handle_style_command(app, rest.trim())?;
        return Ok(());
    }


    if command.eq_ignore_ascii_case("status colour")
        || command.eq_ignore_ascii_case("status color")
        || command.eq_ignore_ascii_case("status-colour")
        || command.eq_ignore_ascii_case("status-color")
    {
        show_status_colour_status(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "status colour ")
        .or_else(|| strip_command_prefix(command, "status color "))
        .or_else(|| strip_command_prefix(command, "status-colour "))
        .or_else(|| strip_command_prefix(command, "status-color "))
    {
        handle_status_colour_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("playlist-multicolour")
        || command.eq_ignore_ascii_case("playlist-multicolor")
        || command.eq_ignore_ascii_case("pmc")
    {
        show_playlist_multicolour_status(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "playlist-multicolour ")
        .or_else(|| strip_command_prefix(command, "playlist-multicolor "))
        .or_else(|| strip_command_prefix(command, "pmc "))
    {
        handle_playlist_multicolour_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("result-multicolour")
        || command.eq_ignore_ascii_case("result-multicolor")
        || command.eq_ignore_ascii_case("results-multicolour")
        || command.eq_ignore_ascii_case("results-multicolor")
        || command.eq_ignore_ascii_case("rmc")
    {
        show_result_multicolour_status(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "result-multicolour ")
        .or_else(|| strip_command_prefix(command, "result-multicolor "))
        .or_else(|| strip_command_prefix(command, "results-multicolour "))
        .or_else(|| strip_command_prefix(command, "results-multicolor "))
        .or_else(|| strip_command_prefix(command, "rmc "))
    {
        handle_result_multicolour_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("servers") {
        let primary_display = app.config.primary_display_name();
        let lines: Vec<String> = app
            .config
            .servers
            .iter()
            .map(|server| {
                let marker = if app.config.is_primary(&server.alias) { "*" } else { " " };
                format!(
                    "{} {} [{}] -> {}",
                    marker, server.name, server.alias, server.base_url
                )
            })
            .collect();

        app.push_message(format!("Primary: {}", primary_display));
        for line in lines {
            app.push_message(line);
        }
        return Ok(());
    }

    if command.eq_ignore_ascii_case("primary") {
        app.push_message(format!("Primary: {}", app.config.primary_display_name()));
        return Ok(());
    }

    if command.eq_ignore_ascii_case("timeout")
        || command.eq_ignore_ascii_case("search-timeout")
        || command.eq_ignore_ascii_case("server-timeout")
    {
        show_search_timeout_status(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "timeout ")
        .or_else(|| strip_command_prefix(command, "search-timeout "))
        .or_else(|| strip_command_prefix(command, "server-timeout "))
    {
        handle_search_timeout_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("queue") || command.eq_ignore_ascii_case("q") {
        app.show_queue_messages();
        return Ok(());
    }

    if command.eq_ignore_ascii_case("queue-follow") || command.eq_ignore_ascii_case("qf") {
        handle_queue_follow_command(app, "toggle")?;
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "queue-follow ")
        .or_else(|| strip_command_prefix(command, "qf "))
    {
        handle_queue_follow_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("gapless") || command.eq_ignore_ascii_case("gap") {
        handle_gapless_command(app, "toggle")?;
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "gapless ")
        .or_else(|| strip_command_prefix(command, "gap "))
    {
        handle_gapless_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("audio") || command.eq_ignore_ascii_case("audio status") {
        show_audio_status(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("audio devices")
        || command.eq_ignore_ascii_case("audio outputs")
        || command.eq_ignore_ascii_case("audio list")
    {
        show_audio_devices(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("audio reset")
        || command.eq_ignore_ascii_case("audio restart")
        || command.eq_ignore_ascii_case("audio reopen")
    {
        handle_audio_reset_command(app)?;
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "audio ") {
        let rest = rest.trim();
        if rest.eq_ignore_ascii_case("status") || rest.eq_ignore_ascii_case("show") {
            show_audio_status(app);
        } else if rest.eq_ignore_ascii_case("devices") || rest.eq_ignore_ascii_case("outputs") || rest.eq_ignore_ascii_case("list") {
            show_audio_devices(app);
        } else if rest.eq_ignore_ascii_case("reset") || rest.eq_ignore_ascii_case("restart") || rest.eq_ignore_ascii_case("reopen") {
            handle_audio_reset_command(app)?;
        } else {
            app.push_message("Unknown audio command. Use audio status, audio devices, or audio reset.".to_string());
        }
        return Ok(());
    }

    if command.eq_ignore_ascii_case("media-keys")

        || command.eq_ignore_ascii_case("mediakeys")
        || command.eq_ignore_ascii_case("media")
        || command.eq_ignore_ascii_case("mk")
    {
        handle_media_keys_command(app, "toggle")?;
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "media-keys ")
        .or_else(|| strip_command_prefix(command, "mediakeys "))
        .or_else(|| strip_command_prefix(command, "media "))
        .or_else(|| strip_command_prefix(command, "mk "))
    {
        handle_media_keys_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("history")
        || command.eq_ignore_ascii_case("played")
        || command.eq_ignore_ascii_case("play-history")
        || command.eq_ignore_ascii_case("playback history")
        || command.eq_ignore_ascii_case("recent tracks")
        || command.eq_ignore_ascii_case("recent history")
        || command.eq_ignore_ascii_case("recent played")
    {
        app.show_recent_messages();
        return Ok(());
    }

    if command.eq_ignore_ascii_case("history clear")
        || command.eq_ignore_ascii_case("played clear")
        || command.eq_ignore_ascii_case("play-history clear")
        || command.eq_ignore_ascii_case("playback history clear")
        || command.eq_ignore_ascii_case("recent tracks clear")
        || command.eq_ignore_ascii_case("recent history clear")
        || command.eq_ignore_ascii_case("recent played clear")
    {
        app.clear_recent_tracks();
        return Ok(());
    }

    if command.eq_ignore_ascii_case("recent clear") {
        app.push_message("Use 'history clear' or 'playback history clear' to clear playback history. Bare 'recent' is reserved for server recent albums.".to_string());
        return Ok(());
    }

    if command.eq_ignore_ascii_case("clear") || command.eq_ignore_ascii_case("c") {
        app.clear_queue();
        return Ok(());
    }

    if command.eq_ignore_ascii_case("queues") || command.eq_ignore_ascii_case("saved-queues") || command.eq_ignore_ascii_case("list-queues") {
        handle_list_saved_queues(app)?;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("save-queue ").or_else(|| command.strip_prefix("sq ")) {
        handle_save_queue_command(app, rest.trim())?;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("load-queue ").or_else(|| command.strip_prefix("lq ")) {
        handle_load_queue_command(app, rest.trim())?;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("delete-queue ").or_else(|| command.strip_prefix("dq ")) {
        handle_delete_queue_command(app, rest.trim())?;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("rename-queue ").or_else(|| command.strip_prefix("rq ")) {
        handle_rename_queue_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("save-queue") || command.eq_ignore_ascii_case("sq") {
        handle_save_queue_command(app, "")?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("session") || command.eq_ignore_ascii_case("session status") {
        handle_session_status_command(app)?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("session save") || command.eq_ignore_ascii_case("save-session") || command.eq_ignore_ascii_case("ss") {
        persist_last_session(app, false)?;
        app.push_message(format!("Saved current queue as last session ({} track(s)).", app.queue.len()));
        return Ok(());
    }

    if command.eq_ignore_ascii_case("session restore")
        || command.eq_ignore_ascii_case("restore-session")
        || command.eq_ignore_ascii_case("rs")
        || command.eq_ignore_ascii_case("resume-session")
        || command.eq_ignore_ascii_case("last-queue")
    {
        handle_restore_session_command(app)?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("session clear") || command.eq_ignore_ascii_case("clear-session") {
        handle_clear_session_command(app)?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("version") || command.eq_ignore_ascii_case("about") {
        show_about(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("now") || command.eq_ignore_ascii_case("np") || command.eq_ignore_ascii_case("status") {
        show_now_playing(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("info") || command.eq_ignore_ascii_case("details") || command.eq_ignore_ascii_case("i") {
        show_info_command(app, "")?;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("info ").or_else(|| command.strip_prefix("details ")).or_else(|| command.strip_prefix("i ")) {
        show_info_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("find")
        || command.eq_ignore_ascii_case("filter")
        || command.eq_ignore_ascii_case("/")
    {
        app.push_message("Usage: find <text>, filter <text>, or / <text> searches the active results/queue/saved/history list without changing it.".to_string());
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "find ")
        .or_else(|| strip_command_prefix(command, "filter "))
        .or_else(|| strip_command_prefix(command, "/ "))
    {
        handle_find_command(app, rest.trim());
        return Ok(());
    }

    if command.eq_ignore_ascii_case("cls")
        || command.eq_ignore_ascii_case("clear-screen")
        || command.eq_ignore_ascii_case("clear-console")
    {
        app.messages.clear();
        app.push_message("Console cleared. Queue, playback, results, and history were not changed.".to_string());
        return Ok(());
    }

    if command.eq_ignore_ascii_case("sort") {
        show_sort_help(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "sort ") {
        handle_sort_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("unshuffle")
        || command.eq_ignore_ascii_case("unsh")
        || command.eq_ignore_ascii_case("shuffle restore")
        || command.eq_ignore_ascii_case("shuffle-order restore")
        || command.eq_ignore_ascii_case("shuffle order restore")
    {
        restore_visible_queue_order_command(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("so")
        || command.eq_ignore_ascii_case("shuffle-order")
        || command.eq_ignore_ascii_case("shuffle order")
    {
        toggle_shuffle_order_mode(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "so ")
        .or_else(|| strip_command_prefix(command, "shuffle-order "))
        .or_else(|| strip_command_prefix(command, "shuffle order "))
    {
        handle_shuffle_order_command(app, rest.trim());
        return Ok(());
    }

    if command.eq_ignore_ascii_case("shuffle") || command.eq_ignore_ascii_case("sh") {
        toggle_shuffle_mode(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "shuffle ")
        .or_else(|| strip_command_prefix(command, "sh "))
    {
        handle_shuffle_mode_or_reorder_command(app, rest.trim());
        return Ok(());
    }

    if command.eq_ignore_ascii_case("dedupe")
        || command.eq_ignore_ascii_case("dedupe queue")
        || command.eq_ignore_ascii_case("queue-dedupe")
        || command.eq_ignore_ascii_case("queue dedupe")
    {
        handle_dedupe_queue_command(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("clear-played")
        || command.eq_ignore_ascii_case("remove-played")
        || command.eq_ignore_ascii_case("trim-before")
        || command.eq_ignore_ascii_case("queue trim-before")
    {
        handle_trim_queue_before_current_command(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("clear-upcoming")
        || command.eq_ignore_ascii_case("trim-after")
        || command.eq_ignore_ascii_case("queue trim-after")
    {
        handle_trim_queue_after_current_command(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("repeat") || command.eq_ignore_ascii_case("rp") {
        app.push_message(format!("Repeat mode: {}. Use repeat off, repeat one, or repeat all.", app.repeat_mode.label()));
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("repeat ").or_else(|| command.strip_prefix("rp ")) {
        handle_repeat_command(app, rest.trim());
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("remove ").or_else(|| command.strip_prefix("rm ")) {
        handle_remove_command(app, rest.trim())?;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("r ") {
        if looks_like_queue_number_args(rest.trim()) {
            handle_remove_command(app, rest.trim())?;
            return Ok(());
        }
    }

    if let Some(rest) = command.strip_prefix("star ") {
        handle_star_command(app, rest.trim(), true).await?;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("unstar ") {
        handle_star_command(app, rest.trim(), false).await?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("download") || command.eq_ignore_ascii_case("dl") {
        show_download_help(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("download-path") || command.eq_ignore_ascii_case("dl-path") {
        show_download_path_status(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "download-path ")
        .or_else(|| strip_command_prefix(command, "dl-path "))
    {
        handle_download_path_command(app, rest.trim())?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("download-overwrite") || command.eq_ignore_ascii_case("dl-overwrite") {
        show_download_overwrite_status(app);
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "download-overwrite ")
        .or_else(|| strip_command_prefix(command, "dl-overwrite "))
    {
        handle_download_overwrite_command(app, rest.trim())?;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("download ").or_else(|| command.strip_prefix("dl ")) {
        handle_download_command(app, rest.trim()).await?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("back") || command.eq_ignore_ascii_case("b") {
        handle_back_command(app);
        return Ok(());
    }

    if command == "]" || command.eq_ignore_ascii_case("next-page") || command.eq_ignore_ascii_case("more") {
        show_next_list_page(app);
        return Ok(());
    }

    if command == "[" || command.eq_ignore_ascii_case("prev-page") || command.eq_ignore_ascii_case("previous-page") || command.eq_ignore_ascii_case("less") {
        show_previous_list_page(app);
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("page ") {
        show_numbered_list_page(app, rest.trim())?;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("use ") {
        app.config.set_primary_by_target(rest.trim())?;
        app.persist_config()?;
        app.push_message(format!("Primary server set to {}", app.config.primary_display_name()));
        return Ok(());
    }

    if command.eq_ignore_ascii_case("add-server") || command.eq_ignore_ascii_case("server-add") {
        let wizard = ServerWizard::new_add();
        app.push_message("Starting server setup. Press Esc to cancel.");
        app.push_message(wizard_prompt(&wizard));
        app.wizard = Some(wizard);
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("edit-server ") {
        let target = rest.trim();
        let existing = app
            .config
            .find_server(target)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No such server: {}", target))?;
        let wizard = ServerWizard::new_edit(
            target.to_string(),
            &existing,
            app.config.is_primary(&existing.alias),
        );
        app.push_message(format!("Editing server {} [{}]. Press Esc to cancel.", existing.name, existing.alias));
        app.push_message("Leave a field blank to keep its current value.");
        app.push_message(wizard_prompt(&wizard));
        app.wizard = Some(wizard);
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("remove-server ") {
        let removed = app.config.remove_server_by_target(rest.trim())?;
        app.persist_config()?;
        app.push_message(format!(
            "Removed server {} [{}]. Primary: {}",
            removed.name,
            removed.alias,
            app.config.primary_display_name()
        ));
        return Ok(());
    }

    if command.eq_ignore_ascii_case("ping all") {
        handle_doctor_ping_command(app).await;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("ping ") {
        let target = rest.trim();
        let server = app
            .config
            .find_server(target)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No such server: {}", target))?;
        let client = SubsonicClient::new(server);
        let msg = client.ping().await?;
        app.push_message(msg);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("ping") {
        let server = app
            .config
            .primary_server()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No primary server configured"))?;
        let client = SubsonicClient::new(server);
        let msg = client.ping().await?;
        app.push_message(msg);
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("msg ") {
        app.push_message(rest.to_string());
        return Ok(());
    }

    if command.eq_ignore_ascii_case("quit") || command.eq_ignore_ascii_case("exit") {
        if let Err(error) = persist_last_session(app, false) {
            app.push_message(format!("Warning: could not save last session: {}", error));
        } else {
            app.push_message("Session saved. Exiting.".to_string());
        }
        app.quit_requested = true;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("cache status") {
        show_cache_status(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("cache clear") || command.eq_ignore_ascii_case("cache refresh") {
        clear_wildcard_caches(app);
        return Ok(());
    }

    if command.eq_ignore_ascii_case("doctor") || command.eq_ignore_ascii_case("doc") || command.eq_ignore_ascii_case("diagnostics") || command.eq_ignore_ascii_case("diagnostic") || command.eq_ignore_ascii_case("diag") {
        show_doctor_report(app, false)?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("doctor downloads")
        || command.eq_ignore_ascii_case("doctor download")
        || command.eq_ignore_ascii_case("doctor local")
        || command.eq_ignore_ascii_case("doc downloads")
        || command.eq_ignore_ascii_case("doc download")
        || command.eq_ignore_ascii_case("doc local")
        || command.eq_ignore_ascii_case("diag downloads")
        || command.eq_ignore_ascii_case("diag download")
        || command.eq_ignore_ascii_case("diag local")
    {
        show_doctor_report(app, true)?;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("doctor ping")
        || command.eq_ignore_ascii_case("doctor servers")
        || command.eq_ignore_ascii_case("doc ping")
        || command.eq_ignore_ascii_case("doc servers")
        || command.eq_ignore_ascii_case("diag ping")
        || command.eq_ignore_ascii_case("diag servers")
    {
        handle_doctor_ping_command(app).await;
        return Ok(());
    }

    if command.eq_ignore_ascii_case("doctor all") || command.eq_ignore_ascii_case("doc all") || command.eq_ignore_ascii_case("diag all") {
        show_doctor_report(app, true)?;
        handle_doctor_ping_command(app).await;
        return Ok(());
    }

    if let Some(rest) = command.strip_prefix("move ") {
        handle_move_command(app, rest.trim())?;
        return Ok(());
    }

    if handle_playback_command(app, command)? {
        return Ok(());
    }

    if handle_server_playlist_command(app, None, command).await? {
        return Ok(());
    }

    if let Some((explicit_server, remainder)) = split_server_prefix(app, command) {
        let explicit_server = explicit_server.to_string();
        let remainder = remainder.to_string();
        if handle_server_playlist_command(app, Some(explicit_server.as_str()), remainder.as_str()).await? {
            return Ok(());
        }
    }

    if let Some(rest) = command.strip_prefix("explore ") {
        return handle_explore_command(app, rest.trim()).await;
    }

    if let Some(rest) = command.strip_prefix("x ") {
        if looks_like_positive_number(rest.trim()) {
            return handle_explore_command(app, rest.trim()).await;
        }
    }

    if let Some(rest) = strip_command_prefix(command, "play ").or_else(|| strip_command_prefix(command, "p ")) {
        let results_context = matches!(app.selection_context, Some(SelectionContext::Results))
            || (app.selection_context.is_none() && app.results.is_some());
        if results_context && (looks_like_add_args(rest.trim()) || rest.trim() == "*") {
            return handle_play_results_command(app, rest.trim()).await;
        }
    }

    if let Some(rest) = command.strip_prefix("add ") {
        return handle_add_command(app, rest.trim()).await;
    }

    if let Some(rest) = command.strip_prefix("a ") {
        if looks_like_add_args(rest.trim()) {
            return handle_add_command(app, rest.trim()).await;
        }
    }

    if maybe_start_background_search(app, command) {
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(command, "all ")
        .or_else(|| strip_command_prefix(command, "@all "))
    {
        return execute_all_search_command(app, rest.trim()).await;
    }

    if let Some((explicit_server, remainder)) = split_server_prefix(app, command) {
        let explicit_server = explicit_server.to_string();
        let remainder = remainder.to_string();
        return execute_search_command(app, Some(explicit_server.as_str()), remainder.as_str()).await;
    }

    execute_search_command(app, None, command).await
}


fn strip_command_prefix<'a>(command: &'a str, prefix: &str) -> Option<&'a str> {
    if command.len() < prefix.len() {
        return None;
    }
    let (head, tail) = command.split_at(prefix.len());
    if head.eq_ignore_ascii_case(prefix) {
        Some(tail)
    } else {
        None
    }
}

fn strip_trailing_queue_view_suffix(command: &str) -> (&str, bool) {
    let trimmed = command.trim();
    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() <= 1 {
        return (trimmed, false);
    }

    let Some(last) = parts.last().copied() else {
        return (trimmed, false);
    };
    if !last.eq_ignore_ascii_case("q") && !last.eq_ignore_ascii_case("queue") {
        return (trimmed, false);
    }

    let cut_at = trimmed.rfind(last).unwrap_or(trimmed.len());
    let without_suffix = trimmed[..cut_at].trim_end();
    let first = without_suffix.split_whitespace().next().unwrap_or("");
    let expanded_without_suffix = expand_compact_single_letter_command(without_suffix);
    let expanded_first = expanded_without_suffix.split_whitespace().next().unwrap_or("");
    let suffix_is_for_queue_action = looks_like_number_selector_args(without_suffix, false)
        || first.eq_ignore_ascii_case("add")
        || first.eq_ignore_ascii_case("a")
        || expanded_first.eq_ignore_ascii_case("add")
        || expanded_first.eq_ignore_ascii_case("a");

    if without_suffix.is_empty() || !suffix_is_for_queue_action {
        (trimmed, false)
    } else {
        (without_suffix, true)
    }
}

fn handle_back_command(app: &mut AppState) {
    if matches!(app.selection_context, Some(SelectionContext::Queue)) {
        if let Some(previous) = app.last_non_queue_context.take() {
            match previous {
                SelectionContext::Results if app.results.is_some() => {
                    let title = app.results.as_ref().map(|state| state.title.clone()).unwrap_or_else(|| "results".to_string());
                    app.selection_context = Some(SelectionContext::Results);
                    app.push_message(format!("Returned to results: {}", title));
                    return;
                }
                SelectionContext::SavedQueues if app.saved_queues.is_some() => {
                    app.selection_context = Some(SelectionContext::SavedQueues);
                    app.push_message("Returned to saved queues.".to_string());
                    return;
                }
                SelectionContext::Recent if !app.recent_tracks.is_empty() => {
                    app.selection_context = Some(SelectionContext::Recent);
                    app.push_message("Returned to playback history.".to_string());
                    return;
                }
                _ => {}
            }
        }
        if app.results.is_some() {
            let title = app.results.as_ref().map(|state| state.title.clone()).unwrap_or_else(|| "results".to_string());
            app.selection_context = Some(SelectionContext::Results);
            app.push_message(format!("Returned to results: {}", title));
            return;
        }
    }

    if let Some(title) = app.pop_results() {
        app.push_message(format!("Returned to previous results: {}", title));
    } else {
        app.push_message("No previous results.");
    }
}

fn handle_find_command(app: &mut AppState, query: &str) {
    let query = query.trim();
    if query.is_empty() {
        app.push_message("Usage: find <text>, filter <text>, or / <text> searches the active list without changing it.".to_string());
        return;
    }

    match app.selection_context {
        Some(SelectionContext::Queue) => show_queue_find_matches(app, query),
        Some(SelectionContext::SavedQueues) => show_saved_queue_find_matches(app, query),
        Some(SelectionContext::Recent) => show_recent_find_matches(app, query),
        Some(SelectionContext::Messages) => show_message_find_matches(app, query),
        _ => {
            if app.results.is_some() {
                show_result_find_matches(app, query);
            } else {
                app.push_message("No active list to search. Run a browse/search command, queue, queues, or history first.".to_string());
            }
        }
    }
}

fn show_message_find_matches(app: &mut AppState, query: &str) {
    let matches: Vec<(usize, String)> = app
        .messages
        .iter()
        .enumerate()
        .filter(|(_, message)| wildcard_match_contains(query, message))
        .map(|(idx, message)| (idx + 1, message.clone()))
        .collect();
    if matches.is_empty() {
        app.push_message(format!("No message-log matches for '{}'.", query));
        return;
    }
    app.push_message(format!("Message-log matches for '{}' ({}):", query, matches.len()));
    for (number, message) in matches.into_iter().take(40) {
        app.push_message(format!("{}. {}", number, message));
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortScope {
    Results,
    Queue,
    SavedQueues,
    Recent,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortDirection {
    Asc,
    Desc,
}

impl SortDirection {
    fn label(self) -> &'static str {
        match self {
            SortDirection::Asc => "ascending",
            SortDirection::Desc => "descending",
        }
    }
}

fn show_sort_help(app: &mut AppState) {
    app.push_message("Usage: sort [results|queue|saved|history] <key> [asc|desc]. Without a scope, sort uses the active list.".to_string());
    app.push_message("Keys include title/name, artist, album, server, type, duration, year, genre, count. Examples: sort title, sort queue album, sort queue artist desc, sort saved count desc.".to_string());
}

fn handle_sort_command(app: &mut AppState, args: &str) -> Result<()> {
    let args = args.trim();
    if args.is_empty() {
        show_sort_help(app);
        return Ok(());
    }

    let mut parts: Vec<String> = args.split_whitespace().map(|part| part.to_lowercase()).collect();
    let mut direction = SortDirection::Asc;

    if parts.first().map(|part| is_desc_token(part)).unwrap_or(false) {
        direction = SortDirection::Desc;
        parts.remove(0);
    } else if parts.first().map(|part| is_asc_token(part)).unwrap_or(false) {
        parts.remove(0);
    }

    let mut scope = None;
    if let Some(first) = parts.first() {
        if let Some(parsed_scope) = parse_sort_scope(first) {
            scope = Some(parsed_scope);
            parts.remove(0);
        }
    }

    if parts.first().map(|part| is_desc_token(part)).unwrap_or(false) {
        direction = SortDirection::Desc;
        parts.remove(0);
    } else if parts.first().map(|part| is_asc_token(part)).unwrap_or(false) {
        parts.remove(0);
    }

    if parts.last().map(|part| is_desc_token(part)).unwrap_or(false) {
        direction = SortDirection::Desc;
        parts.pop();
    } else if parts.last().map(|part| is_asc_token(part)).unwrap_or(false) {
        parts.pop();
    }

    let key = parts.first().map(|part| part.as_str()).unwrap_or("title");
    let scope = match scope {
        Some(scope) => scope,
        None => active_sort_scope(app)?,
    };

    match scope {
        SortScope::Results => sort_current_results(app, key, direction),
        SortScope::Queue => sort_queue(app, key, direction),
        SortScope::SavedQueues => sort_saved_queues(app, key, direction),
        SortScope::Recent => sort_recent_tracks(app, key, direction),
    }
}

fn is_desc_token(value: &str) -> bool {
    matches!(value, "desc" | "descending" | "reverse" | "reversed")
}

fn is_asc_token(value: &str) -> bool {
    matches!(value, "asc" | "ascending")
}

fn parse_sort_scope(value: &str) -> Option<SortScope> {
    match value {
        "result" | "results" => Some(SortScope::Results),
        "queue" | "q" => Some(SortScope::Queue),
        "saved" | "saved-queue" | "saved-queues" | "queues" => Some(SortScope::SavedQueues),
        "history" | "played" | "play-history" | "recent" => Some(SortScope::Recent),
        _ => None,
    }
}

fn active_sort_scope(app: &AppState) -> Result<SortScope> {
    match app.selection_context {
        Some(SelectionContext::Queue) => Ok(SortScope::Queue),
        Some(SelectionContext::SavedQueues) => Ok(SortScope::SavedQueues),
        Some(SelectionContext::Recent) => Ok(SortScope::Recent),
        Some(SelectionContext::Results) => Ok(SortScope::Results),
        Some(SelectionContext::Messages) => Err(anyhow::anyhow!("Message log cannot be sorted. Use view results, view queue, view saved, or view history first.")),
        Some(SelectionContext::Help) => Err(anyhow::anyhow!("Command help cannot be sorted. Use view results, view queue, view saved, or view history first.")),
        None => {
            if app.results.is_some() {
                Ok(SortScope::Results)
            } else if !app.queue.is_empty() {
                Ok(SortScope::Queue)
            } else {
                Err(anyhow::anyhow!("No active list to sort. Run a browse/search command, queue, queues, or history first."))
            }
        }
    }
}

fn sort_current_results(app: &mut AppState, key: &str, direction: SortDirection) -> Result<()> {
    let Some(results) = app.results.as_mut() else {
        app.push_message("No current results to sort.".to_string());
        return Ok(());
    };
    validate_result_sort_key(key)?;
    let count = results.items.len();
    results.items.sort_by(|a, b| compare_sort_values(result_sort_key(a, key), result_sort_key(b, key), direction));
    results.page_start = 0;
    app.selection_context = Some(SelectionContext::Results);
    app.push_message(format!("Sorted {} result item(s) by {} ({}).", count, key, direction.label()));
    app.push_current_result_page();
    Ok(())
}

fn sort_queue(app: &mut AppState, key: &str, direction: SortDirection) -> Result<()> {
    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return Ok(());
    }
    validate_track_sort_key(key)?;
    app.shuffle_original_order = None;
    app.reset_shuffle_play_order();
    let current_key = app
        .current_queue_index
        .and_then(|idx| app.queue.get(idx))
        .map(queue_track_key);
    app.queue.sort_by(|a, b| compare_sort_values(track_sort_key(a, key), track_sort_key(b, key), direction));
    if let Some(key_value) = current_key {
        app.current_queue_index = app.queue.iter().position(|track| queue_track_key(track) == key_value);
    }
    if app.current_queue_index.is_none() && !app.queue.is_empty() {
        app.current_queue_index = Some(0);
    }
    if let Some(index) = app.current_queue_index {
        app.ensure_queue_index_visible(index);
    }
    app.enter_queue_context();
    queue_playback_plan_changed(app);
    app.push_message(format!("Sorted queue by {} ({}). Playback was not restarted.", key, direction.label()));
    app.show_queue_messages();
    Ok(())
}

fn sort_saved_queues(app: &mut AppState, key: &str, direction: SortDirection) -> Result<()> {
    let Some(saved) = app.saved_queues.as_mut() else {
        app.push_message("No saved queue list to sort. Use 'queues' first.".to_string());
        return Ok(());
    };
    validate_saved_queue_sort_key(key)?;
    let count = saved.entries.len();
    saved.entries.sort_by(|a, b| compare_sort_values(saved_queue_sort_key(a, key), saved_queue_sort_key(b, key), direction));
    saved.page_start = 0;
    app.selection_context = Some(SelectionContext::SavedQueues);
    app.push_message(format!("Sorted {} saved queue item(s) by {} ({}).", count, key, direction.label()));
    app.push_current_saved_queue_page();
    Ok(())
}

fn sort_recent_tracks(app: &mut AppState, key: &str, direction: SortDirection) -> Result<()> {
    if app.recent_tracks.is_empty() {
        app.push_message("No playback-history tracks in this session.".to_string());
        return Ok(());
    }
    validate_track_sort_key(key)?;
    app.recent_tracks.sort_by(|a, b| compare_sort_values(track_sort_key(a, key), track_sort_key(b, key), direction));
    app.recent_page_start = 0;
    app.selection_context = Some(SelectionContext::Recent);
    app.push_message(format!("Sorted playback history by {} ({}).", key, direction.label()));
    app.show_recent_messages();
    Ok(())
}

fn compare_sort_values(a: String, b: String, direction: SortDirection) -> std::cmp::Ordering {
    let ord = a.cmp(&b);
    match direction {
        SortDirection::Asc => ord,
        SortDirection::Desc => ord.reverse(),
    }
}

fn validate_result_sort_key(key: &str) -> Result<()> {
    match key {
        "title" | "name" | "artist" | "album" | "details" | "subtitle" | "server" | "type" | "kind" | "id" => Ok(()),
        _ => Err(anyhow::anyhow!("Unsupported results sort key '{}'. Try title, type, artist, album, server, or id.", key)),
    }
}

fn validate_track_sort_key(key: &str) -> Result<()> {
    match key {
        "title" | "name" | "artist" | "album" | "server" | "genre" | "year" | "duration" | "length" | "track" | "track-number" | "number" | "bitrate" | "bit-rate" | "id" => Ok(()),
        _ => Err(anyhow::anyhow!("Unsupported track/queue sort key '{}'. Try title, artist, album, server, genre, year, duration, track, bitrate, or id.", key)),
    }
}

fn validate_saved_queue_sort_key(key: &str) -> Result<()> {
    match key {
        "title" | "name" | "count" | "tracks" | "server" | "source" | "first" | "track" | "path" => Ok(()),
        _ => Err(anyhow::anyhow!("Unsupported saved-queue sort key '{}'. Try name, count, server, first, or path.", key)),
    }
}

fn result_sort_key(item: &SearchResultItem, key: &str) -> String {
    match key {
        "type" | "kind" => format!("{:02}|{}", result_kind_rank(item.kind), item.title.to_lowercase()),
        "artist" | "album" | "details" | "subtitle" => format!("{}|{}", item.subtitle.to_lowercase(), item.title.to_lowercase()),
        "server" => format!("{}|{}", item.server_alias.to_lowercase(), item.title.to_lowercase()),
        "id" => format!("{}|{}", item.target_id.as_deref().unwrap_or("").to_lowercase(), item.title.to_lowercase()),
        "title" | "name" | _ => item.title.to_lowercase(),
    }
}

fn result_kind_rank(kind: ResultKind) -> u8 {
    match kind {
        ResultKind::Artist => 1,
        ResultKind::Album => 2,
        ResultKind::Playlist => 3,
        ResultKind::Track => 4,
        ResultKind::Genre => 5,
        ResultKind::Section => 6,
    }
}

fn track_sort_key(track: &QueueTrack, key: &str) -> String {
    match key {
        "artist" => format!("{}|{}|{}", track.artist.to_lowercase(), track.album.to_lowercase(), padded_option_number(track.track_number)),
        "album" => format!("{}|{}|{}", track.album.to_lowercase(), padded_option_number(track.track_number), track.title.to_lowercase()),
        "server" => format!("{}|{}", track.server_alias.to_lowercase(), track.title.to_lowercase()),
        "genre" => format!("{}|{}", track.genre.as_deref().unwrap_or("").to_lowercase(), track.title.to_lowercase()),
        "year" => format!("{}|{}", padded_option_number(track.year), track.title.to_lowercase()),
        "duration" | "length" => format!("{}|{}", padded_option_number(track.duration_seconds), track.title.to_lowercase()),
        "track" | "track-number" | "number" => format!("{}|{}", padded_option_number(track.track_number), track.title.to_lowercase()),
        "bitrate" | "bit-rate" => format!("{}|{}", padded_option_number(track.bit_rate_kbps), track.title.to_lowercase()),
        "id" => format!("{}|{}", track.id.to_lowercase(), track.title.to_lowercase()),
        "title" | "name" | _ => format!("{}|{}|{}", track.title.to_lowercase(), track.artist.to_lowercase(), track.album.to_lowercase()),
    }
}

fn saved_queue_sort_key(entry: &SavedQueueListEntry, key: &str) -> String {
    match key {
        "count" | "tracks" => format!("{}|{}", padded_number(entry.track_count as u64), entry.name.to_lowercase()),
        "server" | "source" => format!("{}|{}", entry.sources.join(",").to_lowercase(), entry.name.to_lowercase()),
        "first" | "track" => format!("{}|{}", entry.first_track_label.as_deref().unwrap_or("").to_lowercase(), entry.name.to_lowercase()),
        "path" => format!("{}|{}", entry.path.display().to_string().to_lowercase(), entry.name.to_lowercase()),
        "title" | "name" | _ => entry.name.to_lowercase(),
    }
}

fn padded_number(value: u64) -> String {
    format!("{:020}", value)
}

fn padded_option_number(value: Option<u64>) -> String {
    value.map(padded_number).unwrap_or_else(|| "99999999999999999999".to_string())
}

fn list_find_matches(query: &str, rows: Vec<(usize, String)>) -> (usize, Vec<String>) {
    let mut matches = Vec::new();
    let mut total = 0usize;
    for (number, label) in rows {
        if query_matches_title(query, &label) {
            total += 1;
            if matches.len() < 50 {
                matches.push(format!("{}. {}", number, label));
            }
        }
    }
    (total, matches)
}

fn show_result_find_matches(app: &mut AppState, query: &str) {
    let Some(results) = app.results.as_ref() else {
        app.push_message("No active results list to search.".to_string());
        return;
    };
    let title = results.title.clone();
    let rows: Vec<(usize, String)> = results
        .items
        .iter()
        .enumerate()
        .map(|(idx, item)| {
            let label = format!(
                "{} [{:?}] {} {}",
                app.result_item_label(item),
                item.kind,
                item.server_alias,
                item.target_id.as_deref().unwrap_or("")
            );
            (idx + 1, label)
        })
        .collect();
    let (total, lines) = list_find_matches(query, rows);
    push_find_result_lines(app, &format!("results: {}", title), query, total, lines);
}

fn show_queue_find_matches(app: &mut AppState, query: &str) {
    let rows: Vec<(usize, String)> = app
        .queue
        .iter()
        .enumerate()
        .map(|(idx, track)| {
            let marker = if Some(idx) == app.current_queue_index { "> " } else { "" };
            (
                idx + 1,
                format!(
                    "{}{} {} {} {} {} {}",
                    marker,
                    app.track_label(track),
                    track.id,
                    track.suffix.as_deref().unwrap_or(""),
                    track.content_type.as_deref().unwrap_or(""),
                    track.genre.as_deref().unwrap_or(""),
                    track.server_alias
                ),
            )
        })
        .collect();
    let (total, lines) = list_find_matches(query, rows);
    push_find_result_lines(app, "queue", query, total, lines);
}

fn show_saved_queue_find_matches(app: &mut AppState, query: &str) {
    let Some(state) = app.saved_queues.as_ref() else {
        app.push_message("No saved queue list to search. Use 'queues' first.".to_string());
        return;
    };
    let rows: Vec<(usize, String)> = state
        .entries
        .iter()
        .enumerate()
        .map(|(idx, entry)| {
            let label = format!(
                "{} {} {} {}",
                entry.name,
                app.saved_queue_entry_suffix(entry),
                entry.sources.join(" "),
                entry.first_track_label.as_deref().unwrap_or("")
            );
            (idx + 1, label)
        })
        .collect();
    let (total, lines) = list_find_matches(query, rows);
    push_find_result_lines(app, "saved queues", query, total, lines);
}

fn show_recent_find_matches(app: &mut AppState, query: &str) {
    let rows: Vec<(usize, String)> = app
        .recent_tracks
        .iter()
        .enumerate()
        .map(|(idx, track)| {
            (
                idx + 1,
                format!(
                    "{} {} {} {} {} {}",
                    app.track_label(track),
                    track.id,
                    track.suffix.as_deref().unwrap_or(""),
                    track.content_type.as_deref().unwrap_or(""),
                    track.genre.as_deref().unwrap_or(""),
                    track.server_alias
                ),
            )
        })
        .collect();
    let (total, lines) = list_find_matches(query, rows);
    push_find_result_lines(app, "playback history", query, total, lines);
}

fn push_find_result_lines(app: &mut AppState, context: &str, query: &str, total: usize, lines: Vec<String>) {
    if total == 0 {
        app.push_message(format!("Find '{}' in {}: no matches.", query, context));
        return;
    }
    app.push_message(format!(
        "Find '{}' in {}: {} match(es). Numbers shown are absolute and can be used with commands such as info, add/a, dl, star, remove, lq, or dq where that context supports them.",
        query,
        context,
        total
    ));
    for line in lines {
        app.push_message(line);
    }
    if total > 50 {
        app.push_message(format!("Showing first 50 of {} matches. Use a more specific find query to narrow the list.", total));
    }
}

fn show_search_timeout_status(app: &mut AppState) {
    app.push_message(format!(
        "Default search timeout: {}s. Per-server overrides:",
        app.config.search_timeout_seconds.clamp(5, 600)
    ));
    if app.config.servers.is_empty() {
        app.push_message("  no servers configured".to_string());
        return;
    }
    let lines: Vec<String> = app
        .config
        .servers
        .iter()
        .map(|server| {
            format!(
                "  {} [{}]: {}s",
                server.name,
                server.alias,
                server_timeout_seconds(server)
            )
        })
        .collect();
    for line in lines {
        app.push_message(line);
    }
}

fn handle_search_timeout_command(app: &mut AppState, value: &str) -> Result<()> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("status") || value.eq_ignore_ascii_case("show") {
        show_search_timeout_status(app);
        return Ok(());
    }

    let parts: Vec<&str> = value.split_whitespace().collect();
    match parts.as_slice() {
        [seconds] => {
            let timeout = parse_timeout_seconds(seconds)?;
            app.config.search_timeout_seconds = timeout;
            for server in &mut app.config.servers {
                server.search_timeout_seconds = timeout;
            }
            app.persist_config()?;
            app.push_message(format!("Search timeout set to {}s for default and all configured servers.", timeout));
        }
        [target, seconds] => {
            let timeout = parse_timeout_seconds(seconds)?;
            let server = app
                .config
                .servers
                .iter_mut()
                .find(|server| server.alias.eq_ignore_ascii_case(target) || server.name.eq_ignore_ascii_case(target))
                .ok_or_else(|| anyhow::anyhow!("No such server: {}", target))?;
            server.search_timeout_seconds = timeout;
            let alias = server.alias.clone();
            app.persist_config()?;
            app.push_message(format!("Search timeout for [{}] set to {}s.", alias, timeout));
        }
        _ => {
            app.push_message("Usage: timeout [seconds], timeout <server> <seconds>, or timeout status.".to_string());
        }
    }
    Ok(())
}

fn parse_timeout_seconds(value: &str) -> Result<u64> {
    let seconds = value.parse::<u64>().map_err(|_| anyhow::anyhow!("Timeout must be a whole number of seconds."))?;
    if !(5..=600).contains(&seconds) {
        anyhow::bail!("Timeout must be between 5 and 600 seconds.");
    }
    Ok(seconds)
}

fn show_queue_follow_status(app: &mut AppState) {
    app.push_message(format!(
        "Queue-follow is {}. When on, queue-affecting result commands show the play queue automatically. Use qf to toggle, qf on/off to force a state, or suffix a command with q/queue once, e.g. a 1 3 5-7 q.",
        on_off(app.config.queue_follow)
    ));
}

fn handle_queue_follow_command(app: &mut AppState, value: &str) -> Result<()> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "on" | "true" | "yes" | "1" => app.config.queue_follow = true,
        "off" | "false" | "no" | "0" => app.config.queue_follow = false,
        "toggle" | "" => app.config.queue_follow = !app.config.queue_follow,
        "status" | "show" | "?" => {
            show_queue_follow_status(app);
            return Ok(());
        }
        other => {
            app.push_message(format!("Unknown queue-follow setting '{}'. Use qf, qf on, qf off, qf toggle, or qf status.", other));
            return Ok(());
        }
    }
    app.persist_config()?;
    app.push_message(format!("Queue-follow is now {}.", on_off(app.config.queue_follow)));
    Ok(())
}

fn show_gapless_status(app: &mut AppState) {
    app.push_message(format!(
        "Gapless playback is {}. Use gap/gapless to toggle, gap on/off or gapless on/off to force a state, or gap status / gapless status to inspect it. When enabled, DISC preloads, validates, and near the end of a track arms the likely next decoder for a smoother handoff; gapless off keeps the safe single-track path.",
        on_off(app.config.gapless_playback)
    ));
}

fn handle_gapless_command(app: &mut AppState, value: &str) -> Result<()> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "on" | "true" | "yes" | "1" => app.config.gapless_playback = true,
        "off" | "false" | "no" | "0" => app.config.gapless_playback = false,
        "toggle" | "" => app.config.gapless_playback = !app.config.gapless_playback,
        "status" | "show" | "?" => {
            show_gapless_status(app);
            return Ok(());
        }
        other => {
            app.push_message(format!("Unknown gapless setting '{}'. Use gap/gapless, gap on/off, gap toggle, or gap status.", other));
            return Ok(());
        }
    }
    if app.config.gapless_playback {
        schedule_gapless_preload(app);
    } else if let Some(engine) = app.playback.as_ref() {
        engine.prepare_next(None);
    }
    app.persist_config()?;
    app.push_message(format!("Gapless playback is now {}. When enabled, DISC preloads, validates, and near the end of a track arms the likely next decoder for a smoother handoff; use gap off / gapless off to return to the safe single-track path.", on_off(app.config.gapless_playback)));
    Ok(())
}

fn current_audio_status(app: &AppState) -> PlaybackAudioStatus {
    match app.playback.as_ref() {
        Some(engine) => engine.audio_status(),
        None => PlaybackEngine::probe_audio_status(),
    }
}

fn show_audio_status(app: &mut AppState) {
    let status = current_audio_status(app);
    let engine_state = if app.playback.is_some() {
        "available".to_string()
    } else {
        format!(
            "unavailable ({})",
            app.playback_init_error
                .as_deref()
                .unwrap_or("audio output could not be initialised")
        )
    };
    app.push_message(format!("Audio engine: {}.", engine_state));
    app.push_message(format!(
        "Audio output: active={} | current OS default={}",
        status.active_output.as_deref().unwrap_or("unknown/not opened yet"),
        status.default_output.as_deref().unwrap_or("none reported")
    ));
    app.push_message(format!(
        "Detected output device(s): {}",
        if status.output_devices.is_empty() {
            "none reported".to_string()
        } else {
            status.output_devices.join("; ")
        }
    ));
    if let Some(error) = status.output_error {
        app.push_message(format!("Audio device warning: {}", error));
    }
    if let Some(error) = status.last_reset_error {
        app.push_message(format!("Last audio reset warning: {}", error));
    }
    app.push_message("Use audio reset after RDP/local-login, Bluetooth, HDMI, USB DAC, or sleep/wake output changes. Also check Windows Sound > Volume mixer for disc.exe if playback appears active but is silent.".to_string());
}

fn show_audio_devices(app: &mut AppState) {
    let status = current_audio_status(app);
    app.push_message(format!(
        "Current OS default output: {}",
        status.default_output.as_deref().unwrap_or("none reported")
    ));
    if status.output_devices.is_empty() {
        app.push_message("No output devices were reported by the OS audio host.".to_string());
    } else {
        app.push_message(format!("Output devices ({}):", status.output_devices.len()));
        for device in status.output_devices {
            app.push_message(format!("  {}", device));
        }
    }
    if let Some(error) = status.output_error {
        app.push_message(format!("Audio device warning: {}", error));
    }
}

fn handle_audio_reset_command(app: &mut AppState) -> Result<()> {
    if let Some(engine) = app.playback.as_ref().cloned() {
        engine.reset_audio_output()?;
        app.push_message("Audio reset requested: reopening the current OS default output device. If a track was active, DISC will try to resume it at approximately the same position.".to_string());
        app.push_message("If this followed RDP, confirm Windows Sound > Volume mixer routes disc.exe to the laptop speakers/headphones rather than Remote Audio.".to_string());
        return Ok(());
    }

    match PlaybackEngine::new() {
        Ok(engine) => {
            app.playback = Some(Arc::new(engine));
            app.playback_init_error = None;
            app.push_message("Audio engine initialised against the current OS default output device.".to_string());
        }
        Err(error) => {
            let message = error.to_string();
            app.playback_init_error = Some(message.clone());
            app.push_message(format!(
                "Audio reset could not initialise playback: {}. Check Windows Sound > Volume mixer/output device, then run audio reset again or restart DISC locally.",
                message
            ));
        }
    }
    Ok(())
}

fn show_media_keys_status(app: &mut AppState) {
    app.push_message(format!(
        "Media keys are {}. Use media-keys/mk to toggle, mk on/off to force a state, or mk status to inspect it. When enabled, DISC publishes now-playing metadata and responds to OS media controls where the platform supports them.",
        on_off(app.config.media_keys_enabled)
    ));
    if let Some(error) = &app.media_controls_error {
        app.push_message(format!("Media key integration warning: {}", error));
    }
}

fn handle_media_keys_command(app: &mut AppState, value: &str) -> Result<()> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "on" | "true" | "yes" | "1" => app.config.media_keys_enabled = true,
        "off" | "false" | "no" | "0" => app.config.media_keys_enabled = false,
        "toggle" | "" => app.config.media_keys_enabled = !app.config.media_keys_enabled,
        "status" | "show" | "?" => {
            show_media_keys_status(app);
            return Ok(());
        }
        other => {
            app.push_message(format!("Unknown media-key setting '{}'. Use mk, mk on/off, mk toggle, or mk status.", other));
            return Ok(());
        }
    }

    if app.config.media_keys_enabled {
        match MediaIntegration::new() {
            Ok(media) => {
                app.media_controls = Some(media);
                app.media_controls_error = None;
                app.push_message("Media keys and now-playing metadata enabled.".to_string());
            }
            Err(error) => {
                let message = error.to_string();
                app.media_controls = None;
                app.media_controls_error = Some(message.clone());
                app.push_message(format!("Media keys enabled in config, but OS integration is currently unavailable: {}", message));
            }
        }
    } else {
        app.media_controls = None;
        app.media_controls_error = None;
        app.push_message("Media keys and now-playing metadata disabled.".to_string());
    }

    app.persist_config()?;
    Ok(())
}

fn show_messages_visibility_status(app: &mut AppState) {
    app.push_message(format!(
        "Message panel is {}. Use msg/message/messages to toggle, msg on/off to force a state, or msg log to view the full message log in the main panel.",
        on_off(app.config.messages_visible)
    ));
}

fn handle_messages_visibility_command(app: &mut AppState, value: &str) -> Result<()> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "on" | "true" | "yes" | "1" => app.config.messages_visible = true,
        "off" | "false" | "no" | "0" => app.config.messages_visible = false,
        "toggle" | "" => app.config.messages_visible = !app.config.messages_visible,
        "status" | "show" | "?" => {
            show_messages_visibility_status(app);
            return Ok(());
        }
        other => {
            app.push_message(format!("Unknown message-panel setting '{}'. Use msg, msg on, msg off, msg toggle, msg status, or msg log.", other));
            return Ok(());
        }
    }
    app.persist_config()?;
    app.push_message(format!("Message panel is now {}.", on_off(app.config.messages_visible)));
    Ok(())
}

fn show_advanced_status(app: &mut AppState) {
    app.push_message(format!(
        "Verbose status/hints are {}. Use verbose or vt to toggle; verbose on/off also works. v is reserved for volume.",
        on_off(app.config.advanced_status)
    ));
}

fn handle_advanced_status_command(app: &mut AppState, value: &str) -> Result<()> {
    let normalized = value.trim().to_lowercase();
    match normalized.as_str() {
        "on" | "true" | "yes" | "1" => app.config.advanced_status = true,
        "off" | "false" | "no" | "0" => app.config.advanced_status = false,
        "toggle" | "" => app.config.advanced_status = !app.config.advanced_status,
        "status" | "show" | "?" => {
            show_advanced_status(app);
            return Ok(());
        }
        other => {
            app.push_message(format!("Unknown verbose setting '{}'. Use verbose, verbose on, verbose off, verbose toggle, or verbose status.", other));
            return Ok(());
        }
    }
    app.persist_config()?;
    app.push_message(format!("Verbose status/hints are now {}.", on_off(app.config.advanced_status)));
    Ok(())
}

fn show_status_colour_status(app: &mut AppState) {
    let theme = app.theme();
    let effective = if theme.is_multicolour() { "orange" } else { "style green" };
    app.push_message(format!(
        "Status colour is {} (effective default: {}). Use status colour <colour>, or status colour auto/default to follow the current style.",
        status_colour_label(app),
        effective
    ));
    app.push_message("Supported colours: auto/default, named colours, ansi(208), #ff8800, rgb(255,136,0), and cmyk(0,47,100,0).".to_string());
}

fn handle_status_colour_command(app: &mut AppState, value: &str) -> Result<()> {
    let normalized = value.trim().to_lowercase().replace('_', "-");
    if normalized.is_empty() || matches!(normalized.as_str(), "status" | "show" | "?") {
        show_status_colour_status(app);
        return Ok(());
    }
    if matches!(normalized.as_str(), "auto" | "default" | "style" | "reset" | "clear") {
        app.config.status_colour = None;
        app.persist_config()?;
        app.push_message("Status colour reset to automatic style default.".to_string());
        return Ok(());
    }
    let theme = app.theme();
    if parse_status_colour(&normalized, &theme).is_none() {
        app.push_message(format!("Unknown status colour '{}'. Use status colour to see supported colours.", value.trim()));
        return Ok(());
    }
    app.config.status_colour = Some(normalized.clone());
    app.persist_config()?;
    app.push_message(format!("Status colour set to {}.", normalized));
    Ok(())
}

fn show_style_status(app: &mut AppState) {
    let theme = app.theme();
    app.push_message(format!(
        "Current style: {}. Built-in styles: {}.",
        theme.label(),
        style_list_label()
    ));
    app.push_message(format!(
        "Custom styles: {}. File: {}.",
        custom_styles_label(&app.custom_styles),
        app.store.custom_styles_path().display()
    ));
    if let Some(error) = &app.custom_styles_error {
        app.push_message(format!("Custom style load warning: {}", error));
    }
    app.push_message("Use sty/style/theme <id|name> to switch, style reload to reload custom styles, style path to show the file path, or style sample to create an example file.".to_string());
    app.push_message(format!(
        "Playlist multicolour: {}. Use pmc on/off or playlist-multicolour on/off.",
        if app.config.playlist_multicolour { "on" } else { "off" }
    ));
    app.push_message(format!(
        "Result multicolour: {}. Use rmc on/off or result-multicolour on/off.",
        if app.config.result_multicolour { "on" } else { "off" }
    ));
    app.push_message(format!("Status colour: {}. Use status colour <colour> or status colour auto/default.", status_colour_label(app)));
}

fn handle_style_command(app: &mut AppState, requested: &str) -> Result<()> {
    let trimmed = requested.trim();
    if trimmed.eq_ignore_ascii_case("path") || trimmed.eq_ignore_ascii_case("file") {
        app.push_message(format!("Custom styles file: {}", app.store.custom_styles_path().display()));
        return Ok(());
    }
    if trimmed.eq_ignore_ascii_case("reload") || trimmed.eq_ignore_ascii_case("refresh") {
        let (styles, error) = load_custom_styles_from_store(&app.store);
        app.custom_styles = styles;
        app.custom_styles_error = error;
        if let Some(error) = &app.custom_styles_error {
            app.push_message(format!("Custom styles reload failed: {}", error));
        } else {
            app.push_message(format!("Reloaded {} custom style(s).", app.custom_styles.len()));
        }
        return Ok(());
    }
    if trimmed.eq_ignore_ascii_case("custom") || trimmed.eq_ignore_ascii_case("customs") {
        app.push_message(format!("Custom styles: {}.", custom_styles_label(&app.custom_styles)));
        app.push_message(format!("Edit {} and use style reload.", app.store.custom_styles_path().display()));
        return Ok(());
    }
    if trimmed.eq_ignore_ascii_case("sample") || trimmed.eq_ignore_ascii_case("example") || trimmed.eq_ignore_ascii_case("init") {
        write_custom_style_sample(app)?;
        return Ok(());
    }

    let style_name = if let Some(style_name) = resolve_style_token(trimmed) {
        style_name.to_string()
    } else if let Some(custom) = custom_style_by_name(&app.custom_styles, trimmed) {
        format!("custom:{}", custom.name)
    } else {
        app.push_message(format!(
            "Unknown style '{}'. Built-ins: {}. Custom styles: {}.",
            requested,
            style_list_label(),
            custom_styles_label(&app.custom_styles)
        ));
        return Ok(());
    };

    app.config.theme = style_name.clone();
    app.persist_config()?;
    let theme = app.theme();
    app.push_message(format!(
        "Style switched to {} and saved in {}.",
        theme.label(),
        app.store.path().display()
    ));
    if theme.is_multicolour() && !app.config.playlist_multicolour {
        app.push_message("This is a multi-* style, but playlist multicolour is off. Use 'pmc on' to colour play-queue tracks by album.".to_string());
    }
    Ok(())
}

fn write_custom_style_sample(app: &mut AppState) -> Result<()> {
    let path = app.store.custom_styles_path();
    if path.exists() {
        app.push_message(format!("Custom styles file already exists: {}. Edit it and use style reload.", path.display()));
        return Ok(());
    }
    let sample = r##"# DISC custom styles. Use style reload after editing.
# Colour values can be names, ansi(208), #ff8800, rgb(255,136,0), or cmyk(0,47,100,0).

[[styles]]
name = "Custom Amber"
base = "multi-soft"
fg = "#ffb000"
accent = "#ffb000"
secondary = "#ff8800"
panel = "#090807"
danger = "light-red"
warning = "yellow"
queue_palette = ["#ffb000", "#ff8800", "#7db7ff", "#d7c75f"]
"##;
    fs::write(&path, sample)?;
    app.push_message(format!("Wrote example custom styles file: {}. Edit it and use style reload.", path.display()));
    Ok(())
}

fn show_playlist_multicolour_status(app: &mut AppState) {
    let theme = app.theme();
    app.push_message(format!(
        "Playlist multicolour is {}. Current style: {}. Use pmc on/off or playlist-multicolour on/off.",
        if app.config.playlist_multicolour { "on" } else { "off" },
        theme.label()
    ));
}

fn handle_playlist_multicolour_command(app: &mut AppState, value: &str) -> Result<()> {
    let normalized = value.trim().to_lowercase();
    let next = match normalized.as_str() {
        "on" | "yes" | "true" | "1" => true,
        "off" | "no" | "false" | "0" => false,
        "toggle" => !app.config.playlist_multicolour,
        _ => {
            app.push_message("Use pmc on, pmc off, or pmc toggle.".to_string());
            return Ok(());
        }
    };
    app.config.playlist_multicolour = next;
    app.persist_config()?;
    app.push_message(format!(
        "Playlist multicolour {}.",
        if next { "enabled" } else { "disabled" }
    ));
    Ok(())
}

fn show_result_multicolour_status(app: &mut AppState) {
    let theme = app.theme();
    app.push_message(format!(
        "Result multicolour is {}. Current style: {}. Use rmc on/off or result-multicolour on/off.",
        if app.config.result_multicolour { "on" } else { "off" },
        theme.label()
    ));
}

fn handle_result_multicolour_command(app: &mut AppState, value: &str) -> Result<()> {
    let normalized = value.trim().to_lowercase();
    let next = match normalized.as_str() {
        "on" | "yes" | "true" | "1" => true,
        "off" | "no" | "false" | "0" => false,
        "toggle" => !app.config.result_multicolour,
        _ => {
            app.push_message("Use rmc on, rmc off, or rmc toggle.".to_string());
            return Ok(());
        }
    };
    app.config.result_multicolour = next;
    app.persist_config()?;
    app.push_message(format!(
        "Result multicolour {}.",
        if next { "enabled" } else { "disabled" }
    ));
    Ok(())
}

fn show_view_status(app: &mut AppState) {
    app.push_message(format!(
        "Active view: {}. Use view home, view results, view queue, view saved, view history, or view messages.",
        active_context_label(app)
    ));
}

fn handle_view_command(app: &mut AppState, target: &str) -> Result<()> {
    let normalized = target.trim().to_lowercase();
    match normalized.as_str() {
        "" => show_view_status(app),
        "home" | "main" | "dashboard" => {
            app.selection_context = None;
            app.push_message("Showing home panel.".to_string());
        }
        "result" | "results" | "search" | "browse" => {
            if app.results.is_some() {
                app.selection_context = Some(SelectionContext::Results);
                app.push_message("Showing current results panel.".to_string());
            } else {
                app.push_message("No result list is available yet. Try a browse/search command first.".to_string());
            }
        }
        "queue" | "q" | "playqueue" | "play-queue" => {
            app.enter_queue_context();
            app.push_message("Showing play queue panel.".to_string());
        }
        "saved" | "saved-queue" | "saved-queues" | "queues" => {
            if app.saved_queues.is_some() {
                app.selection_context = Some(SelectionContext::SavedQueues);
                app.push_message("Showing saved queues panel.".to_string());
            } else {
                app.push_message("No saved-queue list is loaded. Use queues first.".to_string());
            }
        }
        "history" | "played" | "play-history" | "recent-tracks" => {
            app.selection_context = Some(SelectionContext::Recent);
            app.push_message("Showing playback history panel.".to_string());
        }
        "message" | "messages" | "log" | "message-log" | "console" => {
            app.selection_context = Some(SelectionContext::Messages);
            app.push_message("Showing message log in the main page. Use cls to clear it.".to_string());
        }
        "help" => {
            show_help_index(app);
        }
        "commands" | "command-reference" | "full-help" => {
            show_command_reference(app);
        }
        _ => {
            app.push_message(format!(
                "Unknown view '{}'. Use view home, view results, view queue, view saved, view history, view messages, or view help.",
                target
            ));
        }
    }
    Ok(())
}

fn command_reference_lines() -> Vec<String> {
    include_str!("../../COMMANDS.md")
        .lines()
        .map(|line| line.trim_end().to_string())
        .collect()
}

fn show_command_reference(app: &mut AppState) {
    app.help = Some(HelpState::new("Commands".to_string(), command_reference_lines()));
    app.selection_context = Some(SelectionContext::Help);
    app.push_message("Showing full command reference. Use [ and ] or page <n> to navigate within the reference.".to_string());
}

fn show_help(app: &mut AppState, topic: Option<&str>) {
    let normalized = topic.unwrap_or("").trim().to_lowercase();
    if normalized.is_empty() || normalized == "index" || normalized == "topics" {
        show_help_index(app);
        return;
    }
    if normalized == "all" || normalized == "commands" || normalized == "command" || normalized == "full" || normalized == "command-reference" || normalized == "legacy" {
        show_command_reference(app);
        return;
    }
    if let Some(number) = help_topic_number_from_query(&normalized) {
        if show_home_help_topic(app, number) {
            return;
        }
    }
    match normalized.as_str() {
        "" | "all" => {
            app.push_message("Help topics: help basics | help browse/random | help wildcard | help queue | help shuffle | help find | help sort | help info | help history | help playback/gapless | help saved | help session | help messages | help playlists | help download | help doctor | help star | help style/status-colour | help ui/view | help verbose | help queue-follow | help servers | help keys".to_string());
            app.push_message("Basics: numbers act on the current list; [ and ] page the current list; PageUp/PageDown move previous/next queue track.".to_string());
            app.push_message("Browse: rec/recent defaults to 60 albums; rec n requests n. rnd/random defaults to 60 albums; rnd n requests n albums; rnd t n requests n random tracks. Add g <genre> to random albums/tracks, e.g. rnd 5 g ?rock or rnd t 50 g folk. Other browse commands: genres, playlists, artists, starred, g/genre <query>, artist/ar/art, album/al, track/tr, playlist/pl <query>, search/s <query>. Prefix supported searches with all, e.g. all s deep or all al hazards. Wildcards * and ? are supported.".to_string());
            app.push_message("Queue: queue/q, history/played, add/a <n...>|*, or bare multi-selectors like 1 3 5-7 in results/history/saved contexts; remove/r <n...>, move <from> <to>, dedupe, clear-played, clear-upcoming, clear/c, now/status. Downloads: dl now, dl <n...>, dl *.".to_string());
            app.push_message("Find/polish: find <text>, filter <text>, or / <text> searches the active list without changing it; sort <key> sorts the active list; kill/k cancels a running background search/browse request; cls/clear-screen clears console messages only; verbose or vt toggles verbose hints/status; qf toggles queue-follow; qf on/off forces the state.".to_string());
            app.push_message("Playback: play, pause/p/Space, next/n/PageDown/Right, prev/PageUp/Left, stop, repeat, shuffle/sh, so/shuffle-order, gap/gapless on/off, seek/sk, skip/skp, ff, rew, vol/v (including v90/vol70), mute. Left/Right edit text when command input is active.".to_string());
            app.push_message("Style/UI: sty/style/theme <id|name>, status colour <colour>, pmc on/off, rmc on/off, advanced on/off for extra debug/status panels. Doctor: doctor, doctor downloads, doctor ping.".to_string());
        }
        "basic" | "basics" | "results" | "result" => {
            app.push_message("Result basics:".to_string());
            app.push_message("  Result numbers are absolute across all pages, not per-page.".to_string());
            app.push_message("  In track results, <n> replaces the queue with that track and plays.".to_string());
            app.push_message("  In album/playlist results, <n> replaces the queue with the whole album/playlist and plays.".to_string());
            app.push_message("  Use x<n> or explore <n> to inspect album/playlist tracks instead of playing the whole item.".to_string());
            app.push_message("  Use a<n...>, add <n...>, add *, or bare multiple selectors like 1 3 5-7 to append result items to the queue.".to_string());
            app.push_message("  Use [ and ] to page results, page <n> to jump, and sort <key> to sort the active result list. Page size adapts to the window height.".to_string());
        }
        "browse" | "search" | "s" | "navigation" | "nav" | "rec" | "recent" | "recent album" | "recent albums" | "rnd" | "random" | "artist" | "artists" | "album" | "albums" | "playlist" | "pl" | "genre" | "genres" | "g" | "track" | "tracks" | "song" | "songs" => {
            app.push_message("Browse/search commands:".to_string());
            app.push_message("  rec/recent/recent albums [n] | rnd/random [n] | rnd t [n] random tracks | rnd [n] g <genre> | rnd t [n] g <genre>".to_string());
            app.push_message("  starred/favs | genres | playlists/pls/pl | artists | g/genre/genres <query> | artist/artists/ar/art <query> | album/albums/al <query>".to_string());
            app.push_message("  track/tracks/tr/song/songs <query> | playlist/playlists/pl <query> | search/s <query> | all s <query>".to_string());
            app.push_message("  Add --play, --p, or -p to any search/browse command to append all playable results and start playback; use --replace --play, --rp, or -rp to overwrite the queue first, e.g. all rnd t 30 g folk --rp.".to_string());
            app.push_message("  Server playlist writes: ps <name>, pu <target>, pa <target> [items], pd <target>, pr <target> to <new-name>; pl save/update/add/delete/rename forms also work.".to_string());
            app.push_message("  Wildcards: g f*, playlists *folk*, artist beat? or ar beat?, album haz*, al hazard*, album *purple*, track smoke*.".to_string());
            app.push_message("  Wildcard patterns match anywhere in titles unless already widened with *. Use cache status/cache clear for lightweight wildcard caches.".to_string());
            app.push_message("  Prefix forms still work: <alias> album <query>, <alias> genres, <alias> starred, etc. Aliases h, k, kill, s, al, ar, art, tr, pl, gap, and other command shortcuts are reserved so they are not treated as server prefixes.".to_string());
            app.push_message("  back returns from the queue to the previous list where possible, otherwise to the previous result set, and reports its target.".to_string());
        }
        "playlists" | "server-playlists" | "playlist-save" | "playlist-update" | "playlist-add" | "playlist-delete" | "playlist-rename" | "ps" | "pu" | "pa" | "pd" | "pr" => {
            app.push_message("Server-side Subsonic playlist commands:".to_string());
            app.push_message("  playlists/pls/pl list server playlists; playlist/pl <query> searches them. Use all before supported searches to merge results from every server.".to_string());
            app.push_message("  playlist-save <name> / pl save <name> / ps <name> saves the current queue as a server playlist.".to_string());
            app.push_message("  playlist-update <target> / pl update <target> / pu <target> replaces that playlist with the current queue.".to_string());
            app.push_message("  playlist-add <target> [items] / pa <target> [items] appends current queue or selected items.".to_string());
            app.push_message("  playlist-delete <name|number|id> / pd <target> deletes a server playlist.".to_string());
            app.push_message("  playlist-rename <target> to <new-name> / pr <target> to <new-name> renames it.".to_string());
            app.push_message("  Targets may be a playlist number from playlists, a raw id, or an unambiguous playlist name. Prefix with a server alias to target non-primary servers.".to_string());
        }
        "wildcard" | "wildcards" | "cache" | "caches" => {
            app.push_message("Wildcard search:".to_string());
            app.push_message("  * matches any characters; ? matches one character. Matching is case-insensitive.".to_string());
            app.push_message("  Wildcard patterns match anywhere in titles, so artist beat? or ar beat? can match Beatles/The Beatles.".to_string());
            app.push_message("  genres/playlists/artists use lightweight per-server session caches from getGenres/getPlaylists/getArtists.".to_string());
            app.push_message("  Examples: g f*, genres *folk*, playlists chill*, artist beat? or ar beat?, album *purple*.".to_string());
            app.push_message("  Albums/tracks/search use multiple broad Subsonic search seeds plus local wildcard filtering; album wildcard search also falls back to a bounded album-list scan when the server misses a partial seed. Include at least two non-wildcard characters.".to_string());
            app.push_message("  cache status shows cached counts; cache clear refreshes those cached lists next time.".to_string());
        }
        "queue" | "q" | "queue-follow" | "qf" | "add" | "a" | "remove" | "rm" | "r" | "move" | "clear" => {
            app.push_message("Queue commands:".to_string());
            app.push_message("  queue/q shows the play queue; numbers select absolute queue items; info/i <n> shows queue track details.".to_string());
            app.push_message("  queue-follow/qf is off by default. qf toggles queue-follow; qf on always shows the queue after queue-affecting result commands; qf off disables it; suffix q/queue forces it once, e.g. a 1 3 5-7 q.".to_string());
            app.push_message("  add/a <n...>|* appends current results, playback-history tracks, or saved queues; ranges like a1-5 and compact forms a1, a1,2, a* work. In results/history/saved contexts, bare multiple selectors like 1 3 5-7 append too.".to_string());
            app.push_message("  remove/r <n...> removes queue items; ranges and compact forms like r3-7, r3,5 work.".to_string());
            app.push_message("  move <from> <to> reorders without restarting playback.".to_string());
            app.push_message("  dedupe removes duplicate tracks while preserving the current selection; clear-played removes tracks before the current item; clear-upcoming removes tracks after it.".to_string());
            app.push_message("  history/played/play-history shows tracks played this session; history clear clears that list.".to_string());
            app.push_message("  clear/c empties the queue and stops playback.".to_string());
        }
        "find" | "filter" | "/" | "cls" | "clear-screen" | "clear-console" => {
            app.push_message("Find and console commands:".to_string());
            app.push_message("  find <text>, filter <text>, or / <text> searches the active results, queue, saved-queue, or playback-history list without changing it.".to_string());
            app.push_message("  Wildcards are supported in find queries: find *live*, find sm?ke, / beat?.".to_string());
            app.push_message("  Find results show absolute numbers, so you can still use commands such as info <n>, a<n>, dl <n>, star <n>, remove <n>, lq <n>, or dq <n> depending on the active list.".to_string());
            app.push_message("  cls, clear-screen, or clear-console clears console messages only; it does not clear the play queue. clear/c still clears the queue.".to_string());
        }
        "sort" | "sorting" | "order" | "ordering" => {
            app.push_message("Sort commands:".to_string());
            app.push_message("  sort <key> sorts the active list and redraws it from page 1. Numbers remain absolute after sorting.".to_string());
            app.push_message("  sort desc <key>, sort <key> desc, or sort <scope> <key> desc sorts descending.".to_string());
            app.push_message("  Scopes: results, queue, saved, history. Without a scope, sort uses the active list.".to_string());
            app.push_message("  Common keys: title/name, artist, album, server, type, duration, year, genre, count.".to_string());
            app.push_message("  Examples: sort title, sort results type, sort queue album, sort queue artist desc, sort saved count desc.".to_string());
        }
        "info" | "details" | "detail" | "metadata" => {
            app.push_message("Info/detail commands:".to_string());
            app.push_message("  info/i or info/i now shows the selected/current queue track, including id, server, format metadata, and playback state.".to_string());
            app.push_message("  info <n> shows details for the active list: current results, queue, saved queues, or playback history.".to_string());
            app.push_message("  details <n> is an alias for info <n>. Numbers are absolute across pages, matching add/remove/download commands.".to_string());
            app.push_message("  For album/playlist results, info reminds you that <n> plays the whole item, x<n> explores tracks, and a<n> appends.".to_string());
        }
        "history" | "played" | "play-history" | "playback history" | "recent tracks" | "recent history" | "recent played" => {
            app.push_message("Playback history commands:".to_string());
            app.push_message("  history, played, play-history, playback history, or recent tracks shows tracks started in this session.".to_string());
            app.push_message("  Bare recent is reserved for server recent albums; rec and recent albums also show server recent albums.".to_string());
            app.push_message("  History numbers are absolute across pages; [ and ] page the history list.".to_string());
            app.push_message("  In history view, <n> replaces the queue with that track and plays unless paused.".to_string());
            app.push_message("  In history view, add/a <n...>|* or bare multiple selectors like 1 3 5-7 append history tracks to the queue; dl <n...>|* downloads them; info <n> shows details.".to_string());
            app.push_message("  history clear, playback history clear, or recent tracks clear clears the in-memory playback history for this session.".to_string());
        }
        "playback" | "playback/gapless" | "play" | "transport" | "status" | "now" | "np" | "audio" | "audio status" | "audio devices" | "audio reset" | "vol" | "volume" | "seek" | "sk" | "skip" | "skp" | "ff" | "rew" | "repeat" | "shuffle" | "sh" | "mute" | "gapless" | "gap" => {
            app.push_message("Playback commands:".to_string());
            app.push_message("  play | pause | p | Space | next/n | prev | stop.".to_string());
            app.push_message("  PageDown/PageUp or Right/Left move next/previous queue track when command input is empty. Left/Right move the text cursor when you are editing a command. Ctrl+Q opens queue, Ctrl+B goes back, Ctrl+M toggles mute, Ctrl+C clears queue, Ctrl+Shift+Q quits, Ctrl++/Ctrl+- adjust volume.".to_string());
            app.push_message("  repeat/rp off|one|all; shuffle/sh toggles shuffle playback; with so off the queue stays fixed while the cursor follows a random path; so/shuffle-order controls stable visible shuffle order (standby/on/off); unshuffle/unsh or so restore restores original queue order and turns shuffle off.".to_string());
            app.push_message("  seek/sk <seconds|m:ss|h:mm:ss>, seek/sk +/-<time>, skip/skp <seconds>, skip/skp -<seconds>, ff [seconds], rew [seconds].".to_string());
            app.push_message("  vol/v, vol/v <0-100>, vol/v +/-<n>, mute, unmute. gap/gapless toggles experimental next-track preload/handoff; gap/gapless on/off/status also work.".to_string());
            app.push_message("  audio status shows DISC's active output and the OS default output; audio devices lists detected outputs; audio reset reopens the current default output and tries to resume the active track. Use this after RDP/local-login, Bluetooth, HDMI, USB DAC, or sleep/wake output changes.".to_string());
        }
        "download" | "downloads" | "dl" | "download-path" | "dl-path" | "download-overwrite" | "dl-overwrite" => {
            app.push_message("Download commands:".to_string());
            app.push_message("  download/dl now downloads the selected queue track.".to_string());
            app.push_message("  In results or queue view: dl <n...> downloads selected absolute numbers; dl * downloads the current list. Use info <n> to inspect before downloading.".to_string());
            app.push_message("  Album/playlist downloads try a server ZIP first, but reject tiny placeholder ZIPs and fall back to individual track downloads if unsupported.".to_string());
            app.push_message("  Track file extensions come from Subsonic metadata/headers/audio bytes where available, avoiding generic .bin names.".to_string());
            app.push_message("  Existing files are skipped by default. Add --replace/-replace/-f to overwrite once, or use download-overwrite on/off.".to_string());
            app.push_message("  download-path/dl-path shows the folder; download-path <folder> saves a custom folder; download-path default resets.".to_string());
        }
        "saved" | "saved-queue" | "saved-queues" | "queues" | "rename-queue" | "rq" => {
            app.push_message("Saved queue commands:".to_string());
            app.push_message("  save-queue/sq <name> saves the current queue; save-queue/sq without a name auto-generates one.".to_string());
            app.push_message("  Existing saved queues are not overwritten unless you add --replace, -replace, -r, or -f.".to_string());
            app.push_message("  queues lists saved queues as numbered selectable items with track/server metadata.".to_string());
            app.push_message("  In the saved queue list, <n> or lq <n> loads/plays; a<n> or bare multiple selectors such as 1 3 append saved queues; dq <n> deletes; info <n> shows metadata.".to_string());
            app.push_message("  rename-queue/rq <number-or-name> <new-name> renames a saved queue; use queues then rq 1 new-name for names with spaces.".to_string());
            app.push_message("  Full names still work: lq <name>, dq <name>, rq <name> <new-name>.".to_string());
        }
        "session" | "sessions" | "restore-session" | "rs" | "resume-session" | "last-queue" | "save-session" | "ss" => {
            app.push_message("Session commands:".to_string());
            app.push_message("  The current queue is saved automatically as the last session when the TUI exits.".to_string());
            app.push_message("  restore-session, rs, resume-session, last-queue, or session restore reloads the last queue and starts playback unless paused.".to_string());
            app.push_message("  session status shows whether a last session is available.".to_string());
            app.push_message("  session save, save-session, or ss saves the current queue immediately; session clear deletes the last-session file.".to_string());
        }
        "doctor" | "doc" | "diagnostic" | "diagnostics" | "diag" | "health" | "check" => {
            app.push_message("Diagnostics commands:".to_string());
            app.push_message("  doctor/doc/diag prints local configuration, queue, cache, playback, session, style, and download status.".to_string());
            app.push_message("  doctor/doc/diag downloads also creates/removes a tiny write-test file in the configured download folder.".to_string());
            app.push_message("  doctor/doc/diag ping or ping all checks every configured Subsonic server with ping.view.".to_string());
            app.push_message("  doctor/doc/diag all runs local diagnostics and then pings all configured servers.".to_string());
        }
        "star" | "stars" | "starred" | "favourites" | "favorites" => {
            app.push_message("Star/favourite commands:".to_string());
            app.push_message("  starred/favs lists starred artists, albums, and tracks.".to_string());
            app.push_message("  star <n...>, unstar <n...>, star *, unstar * act on current results or queue view.".to_string());
            app.push_message("  star now / unstar now update the selected queue track.".to_string());
            app.push_message("  Subsonic can star tracks, albums, and artists directly; playlists/genres are skipped.".to_string());
        }
        "style" | "styles" | "theme" | "themes" | "sty" | "pmc" | "playlist-multicolour" | "playlist-multicolor" | "rmc" | "result-multicolour" | "result-multicolor" | "results-multicolour" | "results-multicolor" | "status-colour" | "status-color" | "status colour" | "status color" => {
            app.push_message("Style/theme commands:".to_string());
            app.push_message(format!("  Current style: {}. Available styles: {}.", app.theme().label(), style_list_label()));
            app.push_message("  sty, style, or theme shows the current style and available style IDs.".to_string());
            app.push_message("  sty <id|name>, style <id|name>, or theme <id|name> switches and saves locally.".to_string());
            app.push_message("  Web-parity styles: soft, mid, bright, multi-soft, multi-mid, multi-bright.".to_string());
            app.push_message("  pmc on/off toggles multi-* colouring for play-queue tracks, grouped by album.".to_string());
            app.push_message("  rmc on/off toggles optional multi-* colouring for numbered result/saved-queue lists. Default is off, so search/browse results stay in the primary colour.".to_string());
            app.push_message("  status colour <colour> sets top-panel static labels; auto/default follows the current style, green for monochrome and orange for multi-colour styles. Basic colours include orange, green, yellow, blue, magenta, cyan, red, white, grey/gray, and light/dark variants.".to_string());
            app.push_message("  advanced on/off toggles extra UI/debug/status details without changing playback or queue state.".to_string());
        }
        "ui" | "layout" | "advanced" | "advanced-status" | "debug" | "debug-ui" | "view" | "views" | "panel" | "panels" | "message" | "messages" | "msg" | "message-log" | "log" => {
            app.push_message("UI/layout commands:".to_string());
            app.push_message("  The alpha layout now separates status, top command input, active page, and message log.".to_string());
            app.push_message("  Results, queue, saved queues, and history render in the main page instead of only appending to the console.".to_string());
            app.push_message("  advanced/debug shows extra status/log information and mirrors active pages into the message log; advanced off keeps the cleaner default layout.".to_string());
            app.push_message("  Commands: verbose or vt toggles verbose status/hints; verbose on/off also works; msg/message/messages toggles the bottom message panel; msg on/off forces visibility; msg log opens the full log in the main panel. advanced/debug remain aliases. v is reserved for volume.".to_string());
            app.push_message("  View switching: view home, view results, view queue, view saved, view history, view messages. messages/log opens the message log in the main panel.".to_string());
            app.push_message("  The terminal cursor is placed in the input field where terminal support allows it.".to_string());
        }
        "server" | "servers" | "config" | "primary" | "use" | "ping" | "add-server" | "edit-server" | "remove-server" => {
            app.push_message("Server/config commands:".to_string());
            app.push_message("  servers, primary, use <alias|name>, ping [alias|name], ping all / doctor ping.".to_string());
            app.push_message("  add-server, edit-server <alias|name>, remove-server <alias|name>.".to_string());
            app.push_message("  A server alias is a short nickname for the server, used to make server-specific commands more convenient, e.g. sub rec.".to_string());
            app.push_message("  Bare browse commands target the primary server. Prefix with an alias for a secondary server. Aliases h, k, kill, s, al, ar, art, tr, pl, gap, and other command shortcuts are reserved so they are not treated as server prefixes.".to_string());
        }
        "key" | "keys" | "keyboard" => {
            app.push_message("Keyboard shortcuts:".to_string());
            app.push_message("  Up/Down: command history only.".to_string());
            app.push_message("  Space: play/pause when the command input is empty.".to_string());
            app.push_message("  PageDown/PageUp or Right/Left: next/previous queue track when command input is empty; Left/Right edit command text when input is active.".to_string());
            app.push_message("  [ and ]: previous/next page for the current result, queue, or saved queue list.".to_string());
            app.push_message("  Ctrl+Q or Ctrl+P: show the play queue; Ctrl+B: back; Ctrl+M: toggle mute; Ctrl+C: clear queue; Ctrl+Shift+Q or Esc: quit; Ctrl++/Ctrl+= and Ctrl+- adjust volume by 5.".to_string());
            app.push_message("  Esc: cancel setup wizard, otherwise quit.".to_string());
        }
        other => {
            app.push_message(format!("No help topic named '{}'. Try: help basics, help browse, help random, help wildcard, help queue, help queue-follow, help shuffle, help find, help sort, help info, help history, help playback, help saved, help session, help messages, help download, help doctor, help star, help style, help status-colour, help ui, help view, help playback/gapless, help servers, help keys.", other));
        }
    }
}


fn show_info_command(app: &mut AppState, args: &str) -> Result<()> {
    let target = args.trim();
    if target.is_empty() || target.eq_ignore_ascii_case("now") || target.eq_ignore_ascii_case("current") {
        show_current_track_info(app);
        return Ok(());
    }

    let number = match target.parse::<usize>() {
        Ok(value) if value > 0 => value,
        Ok(_) => {
            app.push_message("Info numbers are 1-based.".to_string());
            return Ok(());
        }
        Err(_) => {
            app.push_message("Usage: info, info now, info <number>, details <number>. The number applies to the active results, queue, saved queue, or playback-history list.".to_string());
            return Ok(());
        }
    };
    let index = number - 1;

    match app.selection_context {
        Some(SelectionContext::Results) if app.results.is_some() => show_result_item_info(app, index),
        Some(SelectionContext::Queue) => show_queue_track_info(app, index),
        Some(SelectionContext::SavedQueues) => show_saved_queue_info(app, index),
        Some(SelectionContext::Recent) => show_recent_track_info(app, index),
        _ if app.results.is_some() => show_result_item_info(app, index),
        _ if !app.queue.is_empty() => show_queue_track_info(app, index),
        _ => app.push_message("Nothing active to inspect. Run a search, use queue, queues, or history first; or use info now for the current queue item.".to_string()),
    }

    Ok(())
}

fn show_current_track_info(app: &mut AppState) {
    match app.current_queue_index.and_then(|idx| app.queue.get(idx).cloned().map(|track| (idx, track))) {
        Some((idx, track)) => {
            app.push_message(format!("Current queue item: {} of {}", idx + 1, app.queue.len()));
            push_track_info(app, &track, Some(idx));
            app.push_message("Actions: play/pause/Space controls playback | dl now downloads | star now/unstar now updates favourites | queue shows context.".to_string());
        }
        None => {
            app.push_message("No current queue item. Select or add a track, album, playlist, saved queue, or playback-history item first.".to_string());
        }
    }
}

fn show_queue_track_info(app: &mut AppState, index: usize) {
    let Some(track) = app.queue.get(index).cloned() else {
        app.push_message(format!("No queue item at number {}. Queue contains {} item(s).", index + 1, app.queue.len()));
        return;
    };
    app.push_message(format!("Queue item {} of {}:", index + 1, app.queue.len()));
    push_track_info(app, &track, Some(index));
    app.push_message("Actions: <n> selects this queue item | remove/r<n> removes | dl <n> downloads | star <n>/unstar <n> updates favourites.".to_string());
}

fn show_recent_track_info(app: &mut AppState, index: usize) {
    let Some(track) = app.recent_tracks.get(index).cloned() else {
        app.push_message(format!("No playback-history track at number {}. History contains {} item(s).", index + 1, app.recent_tracks.len()));
        return;
    };
    app.push_message(format!("Playback-history item {} of {}:", index + 1, app.recent_tracks.len()));
    push_track_info(app, &track, None);
    app.push_message("Actions: <n> replaces the queue with this track | a<n> appends | dl <n> downloads | star <n>/unstar <n> updates favourites.".to_string());
}

fn show_result_item_info(app: &mut AppState, index: usize) {
    let (total, item) = match app.results.as_ref() {
        Some(results) => {
            let total = results.items.len();
            match results.items.get(index).cloned() {
                Some(item) => (total, item),
                None => {
                    app.push_message(format!("No result at number {}. Current results contain {} item(s).", index + 1, total));
                    return;
                }
            }
        }
        None => {
            app.push_message("No current results to inspect.".to_string());
            return;
        }
    };

    app.push_message(format!("Result {} of {}:", index + 1, total));
    app.push_message(format!("  Type: {}", result_kind_label(item.kind)));
    app.push_message(format!("  Title: {}", item.title));
    if !item.subtitle.trim().is_empty() {
        app.push_message(format!("  Details: {}", item.subtitle));
    }
    app.push_message(format!("  Server: {}{}", item.server_alias, if app.is_primary_alias(&item.server_alias) { " (primary)" } else { "" }));
    app.push_message(format!("  Playable: {}", if item.playable { "yes" } else { "no" }));
    app.push_message(format!("  Target id: {}", item.target_id.as_deref().filter(|value| !value.trim().is_empty()).unwrap_or("none")));
    app.push_message(result_info_actions(&item, index));
}

fn show_saved_queue_info(app: &mut AppState, index: usize) {
    let Some(entry) = app.resolve_saved_queue_index(index) else {
        let total = app.saved_queues.as_ref().map(|state| state.entries.len()).unwrap_or(0);
        app.push_message(format!("No saved queue at number {}. Saved queue list contains {} item(s).", index + 1, total));
        return;
    };

    app.push_message(format!("Saved queue {}:", index + 1));
    app.push_message(format!("  Name: {}", entry.name));
    app.push_message(format!("  Tracks: {}", entry.track_count));
    if let Some(current) = entry.current_index.map(|idx| idx + 1) {
        app.push_message(format!("  Saved position: {}", current));
    }
    if !entry.sources.is_empty() {
        app.push_message(format!("  Server aliases: {}", entry.sources.join(", ")));
    }
    if let Some(first) = entry.first_track_label.as_deref() {
        app.push_message(format!("  First track: {}", first));
    }
    app.push_message(format!("  File: {}", entry.path.display()));
    if entry.unreadable {
        app.push_message("  Status: unreadable saved-queue file".to_string());
    }
    app.push_message("Actions: <n>/lq <n> loads and plays | a<n> appends | dq <n> deletes | rq <n> <new-name> renames | dl <n> downloads.".to_string());
}

fn push_track_info(app: &mut AppState, track: &QueueTrack, queue_index: Option<usize>) {
    app.push_message(format!("  Title: {}", track.title));
    app.push_message(format!("  Artist: {}", track.artist));
    app.push_message(format!("  Album: {}", track.album));
    app.push_message(format!("  Server: {}{}", track.server_alias, if app.is_primary_alias(&track.server_alias) { " (primary)" } else { "" }));
    app.push_message(format!("  Track id: {}", track.id));
    if let Some(index) = queue_index {
        let marker = if Some(index) == app.current_queue_index { "yes" } else { "no" };
        app.push_message(format!("  Current queue selection: {}", marker));
    }
    let mut format_parts = Vec::new();
    if let Some(suffix) = track.suffix.as_deref().filter(|value| !value.trim().is_empty()) {
        format_parts.push(format!("suffix {}", suffix));
    }
    if let Some(content_type) = track.content_type.as_deref().filter(|value| !value.trim().is_empty()) {
        format_parts.push(format!("content-type {}", content_type));
    }
    if let Some(duration) = track.duration_seconds {
        format_parts.push(format!("duration {}", format_duration_seconds(duration)));
    }
    if let Some(track_number) = track.track_number {
        format_parts.push(format!("track {}", track_number));
    }
    if let Some(year) = track.year {
        format_parts.push(format!("year {}", year));
    }
    if let Some(genre) = track.genre.as_deref().filter(|value| !value.trim().is_empty()) {
        format_parts.push(format!("genre {}", genre));
    }
    if let Some(bitrate) = track.bit_rate_kbps {
        format_parts.push(format!("{} kbps", bitrate));
    }
    if format_parts.is_empty() {
        app.push_message("  Format metadata: none cached yet".to_string());
    } else {
        app.push_message(format!("  Format metadata: {}", format_parts.join(" • ")));
    }
}

fn result_kind_label(kind: ResultKind) -> &'static str {
    match kind {
        ResultKind::Track => "track",
        ResultKind::Album => "album",
        ResultKind::Artist => "artist",
        ResultKind::Playlist => "playlist",
        ResultKind::Genre => "genre",
        ResultKind::Section => "section",
    }
}

fn result_info_actions(item: &SearchResultItem, index: usize) -> String {
    let number = index + 1;
    match item.kind {
        ResultKind::Track => format!("Actions: {0} replaces queue with this track and plays | a{0} appends | dl {0} downloads | star {0}/unstar {0} updates favourites.", number),
        ResultKind::Album | ResultKind::Playlist => format!("Actions: {0} plays the whole {1} | x{0}/explore {0} shows tracks | a{0} appends | dl {0} downloads.", number, result_kind_label(item.kind)),
        ResultKind::Artist => format!("Actions: {0} shows albums | a{0} appends all artist tracks | dl {0} downloads artist tracks | star {0}/unstar {0} updates favourites.", number),
        ResultKind::Genre => format!("Actions: {0} shows albums in this genre | a{0} appends genre tracks | dl {0} downloads genre tracks.", number),
        ResultKind::Section => "Actions: informational section only.".to_string(),
    }
}

fn format_duration_seconds(seconds: u64) -> String {
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    let secs = seconds % 60;
    if hours > 0 {
        format!("{}:{:02}:{:02}", hours, minutes, secs)
    } else {
        format!("{}:{:02}", minutes, secs)
    }
}

fn playback_state_label(state: PlaybackState) -> &'static str {
    match state {
        PlaybackState::Stopped => "stopped",
        PlaybackState::Playing => "playing",
        PlaybackState::Paused => "paused",
        PlaybackState::Buffering => "buffering",
        PlaybackState::Error => "error",
    }
}

fn format_ms(ms: u64) -> String {
    let total_seconds = ms / 1000;
    let minutes = total_seconds / 60;
    let seconds = total_seconds % 60;
    format!("{}:{:02}", minutes, seconds)
}

fn show_next_list_page(app: &mut AppState) {
    match app.selection_context {
        Some(SelectionContext::Queue) => show_next_queue_page(app),
        Some(SelectionContext::SavedQueues) => show_next_saved_queue_page(app),
        Some(SelectionContext::Recent) => show_next_recent_page(app),
        Some(SelectionContext::Messages) => app.push_message("Messages view is not paged; use advanced on/off or cls to manage the log.".to_string()),
        Some(SelectionContext::Help) => show_next_help_page(app),
        _ => show_next_result_page(app),
    }
}

fn show_previous_list_page(app: &mut AppState) {
    match app.selection_context {
        Some(SelectionContext::Queue) => show_previous_queue_page(app),
        Some(SelectionContext::SavedQueues) => show_previous_saved_queue_page(app),
        Some(SelectionContext::Recent) => show_previous_recent_page(app),
        Some(SelectionContext::Messages) => app.push_message("Messages view is not paged; use advanced on/off or cls to manage the log.".to_string()),
        Some(SelectionContext::Help) => show_previous_help_page(app),
        _ => show_previous_result_page(app),
    }
}

fn show_numbered_list_page(app: &mut AppState, value: &str) -> Result<()> {
    match app.selection_context {
        Some(SelectionContext::Queue) => show_numbered_queue_page(app, value),
        Some(SelectionContext::SavedQueues) => show_numbered_saved_queue_page(app, value),
        Some(SelectionContext::Recent) => show_numbered_recent_page(app, value),
        Some(SelectionContext::Messages) => { app.push_message("Message numbers are informational; use view/results/queue/history/saved to act on lists.".to_string()); Ok(()) },
        Some(SelectionContext::Help) => show_numbered_help_page(app, value),
        _ => show_numbered_result_page(app, value),
    }
}

fn show_next_help_page(app: &mut AppState) {
    let topic_number = app.help.as_ref().and_then(|help| help.topic_number);
    if let Some(topic) = topic_number {
        if topic < HOME_HELP_TOPICS.len() {
            show_home_help_topic(app, topic + 1);
        } else {
            app.push_message("Already at the last help topic.".to_string());
        }
        return;
    }
    let Some(help) = app.help.as_mut() else {
        app.push_message("No help is loaded. Type help or h.".to_string());
        return;
    };
    if help.lines.is_empty() {
        app.push_message("Command reference is empty.".to_string());
        return;
    }
    let next_start = help.page_start.saturating_add(help.page_size);
    if next_start >= help.lines.len() {
        app.push_message("Already at the last command-reference page.".to_string());
        return;
    }
    help.page_start = next_start;
}

fn show_previous_help_page(app: &mut AppState) {
    let topic_number = app.help.as_ref().and_then(|help| help.topic_number);
    if let Some(topic) = topic_number {
        if topic == 0 {
            app.push_message("Already at the help index.".to_string());
        } else if topic == 1 {
            show_help_index(app);
        } else {
            show_home_help_topic(app, topic - 1);
        }
        return;
    }
    let Some(help) = app.help.as_mut() else {
        app.push_message("No help is loaded. Type help or h.".to_string());
        return;
    };
    if help.lines.is_empty() {
        app.push_message("Command reference is empty.".to_string());
        return;
    }
    if help.page_start == 0 {
        app.push_message("Already at the first command-reference page.".to_string());
        return;
    }
    help.page_start = help.page_start.saturating_sub(help.page_size);
}

fn show_numbered_help_page(app: &mut AppState, value: &str) -> Result<()> {
    let page = value
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Page must be a positive number."))?;
    if page == 0 {
        return Err(anyhow::anyhow!("Page numbers are 1-based."));
    }
    let topic_number = app.help.as_ref().and_then(|help| help.topic_number);
    if topic_number.is_some() {
        if show_home_help_topic(app, page) {
            return Ok(());
        }
        app.push_message(format!("No help topic {}. There are {} topic(s).", page, HOME_HELP_TOPICS.len()));
        return Ok(());
    }
    let Some(help) = app.help.as_mut() else {
        app.push_message("No help is loaded. Type help or h.".to_string());
        return Ok(());
    };
    let page_count = help.page_count();
    if page > page_count {
        app.push_message(format!("No command-reference page {}. There are {} page(s).", page, page_count));
        return Ok(());
    }
    help.page_start = (page - 1) * help.page_size;
    Ok(())
}

fn show_next_queue_page(app: &mut AppState) {
    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return;
    }
    let next_start = app.queue_page_start.saturating_add(app.queue_page_size);
    if next_start >= app.queue.len() {
        app.push_message("Already at the last queue page.".to_string());
        return;
    }
    app.queue_page_start = next_start;
    app.show_queue_messages();
}

fn show_previous_queue_page(app: &mut AppState) {
    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return;
    }
    if app.queue_page_start == 0 {
        app.push_message("Already at the first queue page.".to_string());
        return;
    }
    app.queue_page_start = app.queue_page_start.saturating_sub(app.queue_page_size);
    app.show_queue_messages();
}

fn show_numbered_queue_page(app: &mut AppState, value: &str) -> Result<()> {
    let page = value
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Page must be a positive number."))?;
    if page == 0 {
        return Err(anyhow::anyhow!("Page numbers are 1-based."));
    }
    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return Ok(());
    }
    let page_count = app.queue_page_count();
    if page > page_count {
        app.push_message(format!("No queue page {}. There are {} page(s).", page, page_count));
        return Ok(());
    }
    app.queue_page_start = (page - 1) * app.queue_page_size;
    app.show_queue_messages();
    Ok(())
}

fn show_next_saved_queue_page(app: &mut AppState) {
    let Some(saved) = app.saved_queues.as_mut() else {
        app.push_message("No saved queue list. Use 'queues' first.".to_string());
        return;
    };
    if saved.entries.is_empty() {
        app.push_message("No saved queues.".to_string());
        return;
    }
    let next_start = saved.page_start.saturating_add(saved.page_size);
    if next_start >= saved.entries.len() {
        app.push_message("Already at the last saved queue page.".to_string());
        return;
    }
    saved.page_start = next_start;
    app.push_current_saved_queue_page();
}

fn show_previous_saved_queue_page(app: &mut AppState) {
    let Some(saved) = app.saved_queues.as_mut() else {
        app.push_message("No saved queue list. Use 'queues' first.".to_string());
        return;
    };
    if saved.entries.is_empty() {
        app.push_message("No saved queues.".to_string());
        return;
    }
    if saved.page_start == 0 {
        app.push_message("Already at the first saved queue page.".to_string());
        return;
    }
    saved.page_start = saved.page_start.saturating_sub(saved.page_size);
    app.push_current_saved_queue_page();
}

fn show_numbered_saved_queue_page(app: &mut AppState, value: &str) -> Result<()> {
    let page = value
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Page must be a positive number."))?;
    if page == 0 {
        return Err(anyhow::anyhow!("Page numbers are 1-based."));
    }
    let Some(saved) = app.saved_queues.as_mut() else {
        app.push_message("No saved queue list. Use 'queues' first.".to_string());
        return Ok(());
    };
    let page_count = saved.page_count();
    if page > page_count {
        app.push_message(format!("No saved queue page {}. There are {} page(s).", page, page_count));
        return Ok(());
    }
    saved.page_start = (page - 1) * saved.page_size;
    app.push_current_saved_queue_page();
    Ok(())
}

fn show_next_recent_page(app: &mut AppState) {
    if app.recent_tracks.is_empty() {
        app.push_message("No playback-history tracks in this session.".to_string());
        return;
    }
    let next_start = app.recent_page_start.saturating_add(app.recent_page_size);
    if next_start >= app.recent_tracks.len() {
        app.push_message("Already at the last playback-history page.".to_string());
        return;
    }
    app.recent_page_start = next_start;
    app.show_recent_messages();
}

fn show_previous_recent_page(app: &mut AppState) {
    if app.recent_tracks.is_empty() {
        app.push_message("No playback-history tracks in this session.".to_string());
        return;
    }
    if app.recent_page_start == 0 {
        app.push_message("Already at the first playback-history page.".to_string());
        return;
    }
    app.recent_page_start = app.recent_page_start.saturating_sub(app.recent_page_size);
    app.show_recent_messages();
}

fn show_numbered_recent_page(app: &mut AppState, value: &str) -> Result<()> {
    let page = value
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Page must be a positive number."))?;
    if page == 0 {
        return Err(anyhow::anyhow!("Page numbers are 1-based."));
    }
    if app.recent_tracks.is_empty() {
        app.push_message("No playback-history tracks in this session.".to_string());
        return Ok(());
    }
    let page_count = app.recent_page_count();
    if page > page_count {
        app.push_message(format!("No playback-history page {}. There are {} page(s).", page, page_count));
        return Ok(());
    }
    app.recent_page_start = (page - 1) * app.recent_page_size;
    app.show_recent_messages();
    Ok(())
}

fn show_next_result_page(app: &mut AppState) {
    let Some(results) = app.results.as_mut() else {
        app.push_message("No current results.".to_string());
        return;
    };
    if results.items.is_empty() {
        app.push_message("No current results.".to_string());
        return;
    }
    let next_start = results.page_start.saturating_add(results.page_size);
    if next_start >= results.items.len() {
        app.push_message("Already at the last result page.".to_string());
        return;
    }
    results.page_start = next_start;
    app.push_current_result_page();
}

fn show_previous_result_page(app: &mut AppState) {
    let Some(results) = app.results.as_mut() else {
        app.push_message("No current results.".to_string());
        return;
    };
    if results.items.is_empty() {
        app.push_message("No current results.".to_string());
        return;
    }
    if results.page_start == 0 {
        app.push_message("Already at the first result page.".to_string());
        return;
    }
    results.page_start = results.page_start.saturating_sub(results.page_size);
    app.push_current_result_page();
}

fn show_numbered_result_page(app: &mut AppState, value: &str) -> Result<()> {
    let page = value
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Page must be a positive number."))?;
    if page == 0 {
        return Err(anyhow::anyhow!("Page numbers are 1-based."));
    }
    let Some(results) = app.results.as_mut() else {
        app.push_message("No current results.".to_string());
        return Ok(());
    };
    let page_count = results.page_count();
    if page > page_count {
        app.push_message(format!("No result page {}. There are {} page(s).", page, page_count));
        return Ok(());
    }
    results.page_start = (page - 1) * results.page_size;
    app.push_current_result_page();
    Ok(())
}

fn maybe_show_queue_after_queue_change(app: &mut AppState) {
    if app.queue.is_empty() {
        return;
    }
    if !app.force_queue_view_next && !app.config.queue_follow {
        return;
    }
    let index = app.current_queue_index.unwrap_or(0).min(app.queue.len().saturating_sub(1));
    app.ensure_queue_index_visible(index);
    app.enter_queue_context();
}

fn autoplay_after_queue_change(app: &mut AppState, was_empty: bool, force_replace_start: bool) -> Result<()> {
    if app.queue.is_empty() {
        return Ok(());
    }

    maybe_show_queue_after_queue_change(app);

    if app.user_pause_requested {
        app.push_message("Autoplay suppressed because playback is paused. Press Space or use 'play' to start the queued selection.".to_string());
        return Ok(());
    }

    let playback_state = app
        .playback
        .as_ref()
        .map(|engine| engine.snapshot().state)
        .unwrap_or(PlaybackState::Stopped);

    if force_replace_start || matches!(playback_state, PlaybackState::Stopped | PlaybackState::Error) {
        let index = if was_empty || force_replace_start {
            app.current_queue_index.unwrap_or(0)
        } else {
            app.current_queue_index.unwrap_or(0)
        };
        play_queue_index(app, index)?;
    }

    Ok(())
}

fn next_queue_index_for_advance(app: &mut AppState) -> Option<usize> {
    if app.queue.is_empty() {
        return None;
    }

    if app.shuffle_enabled && !app.shuffle_order_enabled {
        return next_random_shuffle_play_index(app);
    }

    let current = app.current_queue_index.unwrap_or(0);
    let next = current.saturating_add(1);
    if next >= app.queue.len() {
        return None;
    }
    Some(next)
}

fn previous_queue_index_for_advance(app: &mut AppState) -> Option<usize> {
    if app.queue.is_empty() {
        return None;
    }

    if app.shuffle_enabled && !app.shuffle_order_enabled {
        return previous_random_shuffle_play_index(app);
    }

    let current = app.current_queue_index.unwrap_or(0);
    if current == 0 {
        return None;
    }
    Some(current - 1)
}

fn shuffle_queue_for_repeat_all(app: &mut AppState) -> Option<usize> {
    if app.shuffle_enabled && !app.shuffle_order_enabled && app.queue.len() > 1 {
        return restart_random_shuffle_play_order(app);
    }
    if app.queue.is_empty() {
        None
    } else {
        Some(0)
    }
}

fn shuffle_play_order_is_current(app: &AppState) -> bool {
    let Some(position) = app.shuffle_play_position else {
        return false;
    };
    if app.shuffle_play_order.len() != app.queue.len() || position >= app.shuffle_play_order.len() {
        return false;
    }
    let Some(current_index) = app.current_queue_index else {
        return false;
    };
    if app.shuffle_play_order[position] != current_index {
        return false;
    }
    let mut seen = HashSet::new();
    app.shuffle_play_order
        .iter()
        .all(|idx| *idx < app.queue.len() && seen.insert(*idx))
}

fn rebuild_random_shuffle_play_order_from_current(app: &mut AppState) {
    app.shuffle_play_order.clear();
    app.shuffle_play_position = None;

    if app.queue.is_empty() {
        return;
    }

    let current = app
        .current_queue_index
        .unwrap_or(0)
        .min(app.queue.len().saturating_sub(1));
    let mut remaining: Vec<usize> = (0..app.queue.len()).filter(|idx| *idx != current).collect();
    remaining.shuffle(&mut rand::thread_rng());

    app.shuffle_play_order.push(current);
    app.shuffle_play_order.extend(remaining);
    app.shuffle_play_position = Some(0);
}

fn ensure_random_shuffle_play_order(app: &mut AppState) {
    if !shuffle_play_order_is_current(app) {
        rebuild_random_shuffle_play_order_from_current(app);
    }
}

fn next_random_shuffle_play_index(app: &mut AppState) -> Option<usize> {
    ensure_random_shuffle_play_order(app);
    let position = app.shuffle_play_position?;
    let next_position = position + 1;
    let next_index = *app.shuffle_play_order.get(next_position)?;
    app.shuffle_play_position = Some(next_position);
    Some(next_index)
}

fn previous_random_shuffle_play_index(app: &mut AppState) -> Option<usize> {
    ensure_random_shuffle_play_order(app);
    let position = app.shuffle_play_position?;
    if position == 0 {
        return None;
    }
    let previous_position = position - 1;
    let previous_index = *app.shuffle_play_order.get(previous_position)?;
    app.shuffle_play_position = Some(previous_position);
    Some(previous_index)
}

fn restart_random_shuffle_play_order(app: &mut AppState) -> Option<usize> {
    if app.queue.is_empty() {
        app.reset_shuffle_play_order();
        return None;
    }

    let current = app.current_queue_index;
    let mut order: Vec<usize> = (0..app.queue.len()).collect();
    order.shuffle(&mut rand::thread_rng());

    if app.queue.len() > 1 {
        if let Some(current_index) = current {
            if order.first().copied() == Some(current_index) {
                if let Some(swap_with) = order.iter().position(|idx| *idx != current_index) {
                    order.swap(0, swap_with);
                }
            }
        }
    }

    let first = order.first().copied();
    app.shuffle_play_order = order;
    app.shuffle_play_position = first.map(|_| 0);
    first
}

fn handle_media_control_events(app: &mut AppState) -> Result<()> {
    let commands = app
        .media_controls
        .as_mut()
        .map(|media| media.drain_commands())
        .unwrap_or_default();

    for command in commands {
        match command {
            MediaKeyCommand::Play => {
                handle_playback_command(app, "play")?;
            }
            MediaKeyCommand::Pause => {
                handle_playback_command(app, "pause")?;
            }
            MediaKeyCommand::Toggle => {
                handle_playback_command(app, "p")?;
            }
            MediaKeyCommand::Next => {
                handle_playback_command(app, "next")?;
            }
            MediaKeyCommand::Previous => {
                handle_playback_command(app, "prev")?;
            }
            MediaKeyCommand::Stop => {
                handle_playback_command(app, "stop")?;
            }
            MediaKeyCommand::SeekBy(ms) => {
                let seconds = (ms.abs() as u64 + 999) / 1000;
                let sign = if ms < 0 { "-" } else { "+" };
                handle_skip_value(app, &format!("{}{}", sign, seconds))?;
            }
            MediaKeyCommand::SeekTo(position) => {
                handle_seek_value(app, &format!("{}", position.as_secs()))?;
            }
            MediaKeyCommand::SetVolume(volume) => {
                let percent = (volume.clamp(0.0, 1.0) * 100.0).round() as u8;
                handle_volume_value(app, &percent.to_string())?;
            }
            MediaKeyCommand::Quit => {
                if let Err(error) = persist_last_session(app, false) {
                    app.push_message(format!("Warning: could not save last session: {}", error));
                }
                app.quit_requested = true;
            }
        }
    }

    Ok(())
}

fn sync_media_controls(app: &mut AppState) {
    if !app.config.media_keys_enabled {
        return;
    }
    if app.media_controls.is_none() {
        match MediaIntegration::new() {
            Ok(media) => {
                app.media_controls = Some(media);
                app.media_controls_error = None;
                app.push_message("Media keys and now-playing metadata enabled.".to_string());
            }
            Err(error) => {
                let message = error.to_string();
                if app.media_controls_error.as_deref() != Some(message.as_str()) {
                    app.push_message(format!("Media keys unavailable: {}", message));
                }
                app.media_controls_error = Some(message);
                return;
            }
        }
    }

    let Some(engine) = app.playback.as_ref().cloned() else {
        return;
    };
    let snapshot = engine.view_snapshot();
    let now_playing = NowPlaying {
        title: snapshot.current.as_ref().map(|track| track.title.clone()),
        artist: snapshot.current.as_ref().map(|track| track.artist.clone()),
        album: snapshot.current.as_ref().map(|track| track.album.clone()),
        duration: snapshot.duration_ms.map(Duration::from_millis),
        position: Some(Duration::from_millis(snapshot.position_ms)),
        status: match snapshot.state {
            PlaybackState::Playing | PlaybackState::Buffering => MediaPlaybackStatus::Playing,
            PlaybackState::Paused => MediaPlaybackStatus::Paused,
            PlaybackState::Stopped | PlaybackState::Error => MediaPlaybackStatus::Stopped,
        },
        volume: snapshot.volume,
    };

    let update_result = app
        .media_controls
        .as_mut()
        .map(|media| media.update(now_playing));
    if let Some(result) = update_result {
        if let Err(error) = result {
            let message = error.to_string();
            if app.media_controls_error.as_deref() != Some(message.as_str()) {
                app.push_message(format!("Could not update now-playing metadata: {}", message));
            }
            app.media_controls_error = Some(message);
        } else {
            app.media_controls_error = None;
        }
    }
}

fn handle_playback_events(app: &mut AppState) -> Result<()> {
    let Some(engine) = app.playback.as_ref().cloned() else {
        return Ok(());
    };

    engine.tick();
    let snapshot = engine.snapshot();
    if let Some(error) = snapshot.error {
        if snapshot.state == PlaybackState::Error {
            app.push_message(format!("Playback error: {}", error));
        } else if error.to_lowercase().starts_with("gapless")
            || error.to_lowercase().contains("gapless handoff")
        {
            app.push_message(format!("Playback notice: {}", error));
        } else {
            app.push_message(format!("Playback warning: {}", error));
        }
    }

    if let Some(started_track) = snapshot.gapless_started.clone() {
        if let Some(index) = queue_index_for_playback_track(app, &started_track) {
            app.current_queue_index = Some(index);
            app.ensure_queue_index_visible(index);
            app.paused_selection_pending = false;
            if app.shuffle_enabled && !app.shuffle_order_enabled {
                if let Some(position) = app.shuffle_play_order.iter().position(|idx| *idx == index) {
                    app.shuffle_play_position = Some(position);
                }
            }
            if let Some(track) = app.queue.get(index).cloned() {
                app.record_recent_track(&track);
            }
            if app.config.advanced_status {
                app.push_message(format!(
                    "Gapless handoff started: {}",
                    app.playback_track_label(&started_track)
                ));
            }
            if matches!(app.selection_context, Some(SelectionContext::Queue)) {
                app.enter_queue_context();
            }
            schedule_gapless_preload(app);
        } else if app.config.advanced_status {
            app.push_message(format!(
                "Gapless handoff started for {}, but DISC could not match it to the current queue.",
                app.playback_track_label(&started_track)
            ));
        }
    }

    if snapshot.just_finished {
        let finished_current = snapshot.current.clone();
        if !playback_snapshot_matches_queue_selection(app, finished_current.as_ref()) {
            if app.config.advanced_status {
                if let Some(track) = finished_current {
                    app.push_message(format!(
                        "Ignored stale finish event for {}; current queue selection has changed.",
                        app.playback_track_label(&track)
                    ));
                } else {
                    app.push_message("Ignored stale finish event with no current playback track.".to_string());
                }
            }
            return Ok(());
        }
        if let Some(track) = finished_current {
            app.push_message(format!("Finished: {}", app.playback_track_label(&track)));
        }
        match app.repeat_mode {
            RepeatMode::One => {
                let replay_index = app.current_queue_index.unwrap_or(0);
                if replay_index < app.queue.len() {
                    app.push_message("Repeat one: restarting current track.".to_string());
                    play_queue_index(app, replay_index)?;
                } else {
                    engine.stop();
                    app.paused_selection_pending = false;
                    app.push_message("End of queue.".to_string());
                }
            }
            RepeatMode::All => {
                if let Some(next_index) = next_queue_index_for_advance(app) {
                    play_queue_index(app, next_index)?;
                } else if !app.queue.is_empty() {
                    let restart_index = shuffle_queue_for_repeat_all(app).unwrap_or(0);
                    app.push_message("Repeat all: restarting queue.".to_string());
                    play_queue_index(app, restart_index)?;
                } else {
                    engine.stop();
                    app.paused_selection_pending = false;
                    app.push_message("End of queue.".to_string());
                }
            }
            RepeatMode::Off => {
                if let Some(next_index) = next_queue_index_for_advance(app) {
                    play_queue_index(app, next_index)?;
                } else {
                    engine.stop();
                    app.paused_selection_pending = false;
                    app.push_message("End of queue.".to_string());
                }
            }
        }
    }

    Ok(())
}

fn handle_playback_command(app: &mut AppState, command: &str) -> Result<bool> {
    let trimmed = command.trim();

    if trimmed.eq_ignore_ascii_case("play") {
        if let Some(engine) = app.playback.as_ref().cloned() {
            let snapshot = engine.snapshot();
            if snapshot.state == PlaybackState::Paused && !app.paused_selection_pending {
                if playback_snapshot_matches_queue_selection(app, snapshot.current.as_ref()) {
                    app.user_pause_requested = false;
                    engine.resume();
                    app.push_message("Playback resumed.".to_string());
                    return Ok(true);
                }
            }
        }
        app.user_pause_requested = false;
        let index = app.current_queue_index.unwrap_or(0);
        play_queue_index(app, index)?;
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("pause") {
        let Some(engine) = app.playback.as_ref().cloned() else {
            push_playback_unavailable(app);
            return Ok(true);
        };
        let snapshot = engine.snapshot();
        app.pending_queue_playback = None;
        if snapshot.state == PlaybackState::Playing {
            engine.pause();
            app.paused_selection_pending = false;
            app.user_pause_requested = true;
            app.push_message("Playback paused. Autoplay is disabled until you use 'play'.".to_string());
        } else if snapshot.state == PlaybackState::Paused {
            app.user_pause_requested = true;
            app.push_message("Playback is already paused. Use 'play' to resume.".to_string());
        } else {
            app.user_pause_requested = true;
            app.push_message("Nothing is currently playing. Autoplay is disabled until you use 'play'.".to_string());
        }
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("p") {
        let Some(engine) = app.playback.as_ref().cloned() else {
            push_playback_unavailable(app);
            return Ok(true);
        };
        let snapshot = engine.snapshot();
        match snapshot.state {
            PlaybackState::Playing => {
                app.pending_queue_playback = None;
                engine.pause();
                app.paused_selection_pending = false;
                app.user_pause_requested = true;
                app.push_message("Playback paused. Autoplay is disabled until you use 'play'.".to_string());
            }
            PlaybackState::Paused => {
                app.user_pause_requested = false;
                if !app.paused_selection_pending
                    && playback_snapshot_matches_queue_selection(app, snapshot.current.as_ref())
                {
                    engine.resume();
                    app.push_message("Playback resumed.".to_string());
                } else {
                    let index = app.current_queue_index.unwrap_or(0);
                    play_queue_index(app, index)?;
                }
            }
            PlaybackState::Stopped | PlaybackState::Error | PlaybackState::Buffering => {
                app.user_pause_requested = false;
                let index = app.current_queue_index.unwrap_or(0);
                play_queue_index(app, index)?;
            }
        }
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("next") || trimmed.eq_ignore_ascii_case("n") {
        if app.queue.is_empty() {
            app.push_message("Queue is empty.".to_string());
            return Ok(true);
        }
        if let Some(next_index) = next_queue_index_for_advance(app) {
            select_queue_index(app, next_index)?;
        } else {
            app.push_message("Already at the end of the queue.".to_string());
        }
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("prev") || trimmed.eq_ignore_ascii_case("previous") {
        if app.queue.is_empty() {
            app.push_message("Queue is empty.".to_string());
            return Ok(true);
        }
        if let Some(previous_index) = previous_queue_index_for_advance(app) {
            select_queue_index(app, previous_index)?;
        } else {
            app.push_message("Already at the start of the queue.".to_string());
        }
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("stop") {
        let Some(engine) = app.playback.as_ref().cloned() else {
            push_playback_unavailable(app);
            return Ok(true);
        };
        app.pending_queue_playback = None;
        engine.stop();
        app.paused_selection_pending = false;
        app.user_pause_requested = false;
        app.push_message("Playback stopped.".to_string());
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("vol") || trimmed.eq_ignore_ascii_case("volume") || trimmed.eq_ignore_ascii_case("v") {
        let Some(engine) = app.playback.as_ref().cloned() else {
            push_playback_unavailable(app);
            return Ok(true);
        };
        app.push_message(format!("Volume: {}", engine.snapshot().volume));
        return Ok(true);
    }

    if let Some(value) = trimmed.strip_prefix("vol ").or_else(|| trimmed.strip_prefix("volume ")).or_else(|| trimmed.strip_prefix("v ")) {
        handle_volume_value(app, value.trim())?;
        return Ok(true);
    }

    if let Some(value) = compact_volume_value(trimmed) {
        handle_volume_value(app, value)?;
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("mute") {
        handle_mute_command(app, true);
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("mute toggle") {
        handle_mute_toggle_command(app);
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("unmute") || trimmed.eq_ignore_ascii_case("mute off") {
        handle_mute_command(app, false);
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("seek") || trimmed.eq_ignore_ascii_case("sk") {
        app.push_message("Usage: seek/sk <seconds|m:ss|h:mm:ss> or seek/sk +/-<seconds|m:ss>. Examples: seek 1:30, sk +30, seek -10".to_string());
        return Ok(true);
    }

    if let Some(value) = trimmed.strip_prefix("seek ").or_else(|| trimmed.strip_prefix("sk ")) {
        handle_seek_value(app, value.trim())?;
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("skip") || trimmed.eq_ignore_ascii_case("skp") {
        app.push_message("Usage: skip/skp <seconds|m:ss> to move forward, or skip/skp -<seconds|m:ss> to move backward. Examples: skip 35, skp -10".to_string());
        return Ok(true);
    }

    if let Some(value) = trimmed.strip_prefix("skip ").or_else(|| trimmed.strip_prefix("skp ")) {
        handle_skip_value(app, value.trim())?;
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("ff") || trimmed.eq_ignore_ascii_case("fast-forward") {
        handle_seek_value(app, "+30")?;
        return Ok(true);
    }

    if let Some(value) = trimmed.strip_prefix("ff ").or_else(|| trimmed.strip_prefix("fast-forward ")) {
        let arg = relative_seek_argument(value.trim(), '+');
        handle_seek_value(app, &arg)?;
        return Ok(true);
    }

    if trimmed.eq_ignore_ascii_case("rew") || trimmed.eq_ignore_ascii_case("rewind") {
        handle_seek_value(app, "-30")?;
        return Ok(true);
    }

    if let Some(value) = trimmed.strip_prefix("rew ").or_else(|| trimmed.strip_prefix("rewind ")) {
        let arg = relative_seek_argument(value.trim(), '-');
        handle_seek_value(app, &arg)?;
        return Ok(true);
    }

    Ok(false)
}

fn refresh_queue_view_if_active(app: &mut AppState, was_queue_view_active: bool) {
    if was_queue_view_active && !app.queue.is_empty() {
        app.show_queue_messages();
    }
}

fn queue_playback_plan_changed(app: &mut AppState) {
    app.pending_queue_playback = None;
    if let Some(engine) = app.playback.as_ref() {
        engine.invalidate_prepared_next();
    }
    schedule_gapless_preload(app);
}

fn play_queue_index(app: &mut AppState, index: usize) -> Result<()> {
    app.pending_queue_playback = None;
    let refresh_queue_view = matches!(app.selection_context, Some(SelectionContext::Queue));

    if app.queue.is_empty() {
        app.push_message("Queue is empty. Select a track or add tracks before using 'play'.".to_string());
        return Ok(());
    }

    let Some(engine) = app.playback.as_ref().cloned() else {
        push_playback_unavailable(app);
        return Ok(());
    };

    let track = app
        .queue
        .get(index)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("No queue item at number {}", index + 1))?;

    let playback_track = playback_track_for_queue_item(app, &track)?;
    let label = app.playback_track_label(&playback_track);
    app.current_queue_index = Some(index);
    app.ensure_queue_index_visible(index);
    app.paused_selection_pending = false;
    app.record_recent_track(&track);
    if app.config.advanced_status {
        app.push_message(format!("Buffering: {}", label));
    }

    engine.play_track(playback_track)?;
    schedule_gapless_preload(app);
    refresh_queue_view_if_active(app, refresh_queue_view);

    Ok(())
}

fn playback_track_for_queue_item(app: &AppState, track: &QueueTrack) -> Result<PlaybackTrack> {
    let client = app.target_client(Some(&track.server_alias))?;
    Ok(PlaybackTrack {
        id: track.id.clone(),
        title: track.title.clone(),
        artist: track.artist.clone(),
        album: track.album.clone(),
        server_alias: track.server_alias.clone(),
        stream_url: client.stream_url(&track.id),
    })
}

fn schedule_gapless_preload(app: &mut AppState) {
    let Some(engine) = app.playback.as_ref().cloned() else {
        return;
    };

    if !app.config.gapless_playback {
        engine.prepare_next(None);
        return;
    }

    let Some(next_index) = next_queue_index_for_gapless_preload(app) else {
        engine.prepare_next(None);
        return;
    };

    let Some(track) = app.queue.get(next_index).cloned() else {
        engine.prepare_next(None);
        return;
    };

    match playback_track_for_queue_item(app, &track) {
        Ok(playback_track) => {
            if app.config.advanced_status {
                app.push_message(format!(
                    "Gapless preload queued for next track: {}",
                    app.playback_track_label(&playback_track)
                ));
            }
            engine.prepare_next(Some(playback_track));
        }
        Err(error) => {
            engine.prepare_next(None);
            if app.config.advanced_status {
                app.push_message(format!("Gapless preload unavailable: {}", error));
            }
        }
    }
}

fn next_queue_index_for_gapless_preload(app: &mut AppState) -> Option<usize> {
    if app.queue.is_empty() {
        return None;
    }

    let current = app.current_queue_index?;
    if current >= app.queue.len() {
        return None;
    }

    if app.repeat_mode == RepeatMode::One {
        return Some(current);
    }

    if app.shuffle_enabled && !app.shuffle_order_enabled {
        ensure_random_shuffle_play_order(app);
        let position = app.shuffle_play_position?;
        if let Some(next_index) = app.shuffle_play_order.get(position + 1).copied() {
            return Some(next_index);
        }
        return None;
    }

    let next = current + 1;
    if next < app.queue.len() {
        Some(next)
    } else if app.repeat_mode == RepeatMode::All {
        Some(0)
    } else {
        None
    }
}

fn push_playback_unavailable(app: &mut AppState) {
    app.push_message(format!(
        "Playback unavailable: {}",
        app.playback_init_error
            .as_deref()
            .unwrap_or("audio output could not be initialised")
    ));
}

fn compact_volume_value(command: &str) -> Option<&str> {
    let trimmed = command.trim();
    let lower = trimmed.to_lowercase();
    for prefix in ["volume", "vol", "v"] {
        if lower.starts_with(prefix) && lower.len() > prefix.len() {
            let value = &trimmed[prefix.len()..];
            let value = value.trim();
            if !value.is_empty()
                && (value.starts_with('+')
                    || value.starts_with('-')
                    || value.chars().next().map(|ch| ch.is_ascii_digit()).unwrap_or(false))
            {
                return Some(value);
            }
        }
    }
    None
}

fn handle_volume_value(app: &mut AppState, value: &str) -> Result<()> {
    let Some(engine) = app.playback.as_ref().cloned() else {
        push_playback_unavailable(app);
        return Ok(());
    };

    let trimmed = value.trim();
    if trimmed.is_empty() {
        app.push_message("Usage: vol <0-100> or vol +/-<amount>".to_string());
        return Ok(());
    }

    let current = engine.snapshot().volume;
    let volume = if let Some(delta) = trimmed.strip_prefix('+') {
        let delta = delta
            .trim()
            .parse::<i16>()
            .map_err(|_| anyhow::anyhow!("Volume adjustment must be a number."))?;
        ((current as i16) + delta).clamp(0, 100) as u8
    } else if let Some(delta) = trimmed.strip_prefix('-') {
        let delta = delta
            .trim()
            .parse::<i16>()
            .map_err(|_| anyhow::anyhow!("Volume adjustment must be a number."))?;
        ((current as i16) - delta).clamp(0, 100) as u8
    } else {
        trimmed
            .parse::<u8>()
            .map_err(|_| anyhow::anyhow!("Volume must be a number from 0 to 100, or +/- a number."))?
            .min(100)
    };

    if volume > 0 {
        app.previous_volume_before_mute = None;
    }
    engine.set_volume(volume);
    app.push_message(format!("Volume set to {}.", volume));
    Ok(())
}

fn handle_mute_toggle_command(app: &mut AppState) {
    let Some(engine) = app.playback.as_ref().cloned() else {
        push_playback_unavailable(app);
        return;
    };
    if engine.snapshot().volume == 0 {
        handle_mute_command(app, false);
    } else {
        handle_mute_command(app, true);
    }
}

fn handle_mute_command(app: &mut AppState, mute: bool) {
    let Some(engine) = app.playback.as_ref().cloned() else {
        push_playback_unavailable(app);
        return;
    };
    let current = engine.snapshot().volume;

    if mute {
        if current > 0 {
            app.previous_volume_before_mute = Some(current);
        }
        engine.set_volume(0);
        app.push_message("Muted. Use 'unmute' to restore volume.".to_string());
    } else {
        let restored = app.previous_volume_before_mute.take().unwrap_or(80).max(1).min(100);
        engine.set_volume(restored);
        app.push_message(format!("Volume restored to {}.", restored));
    }
}

fn handle_seek_value(app: &mut AppState, value: &str) -> Result<()> {
    let Some(engine) = app.playback.as_ref().cloned() else {
        push_playback_unavailable(app);
        return Ok(());
    };

    engine.tick();
    let snapshot = engine.snapshot();
    if matches!(snapshot.state, PlaybackState::Stopped | PlaybackState::Error) || snapshot.current.is_none() {
        app.push_message("Nothing is currently loaded to seek.".to_string());
        return Ok(());
    }

    let target_ms = parse_seek_target_ms(value, snapshot.position_ms, snapshot.duration_ms)?;
    engine.seek(Duration::from_millis(target_ms))?;
    let duration = snapshot
        .duration_ms
        .map(format_ms)
        .unwrap_or_else(|| "--:--".to_string());
    app.push_message(format!("Seeked to {}/{}.", format_ms(target_ms), duration));
    Ok(())
}
fn handle_skip_value(app: &mut AppState, value: &str) -> Result<()> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        app.push_message("Usage: skip/skp <seconds|m:ss> to move forward, or skip/skp -<seconds|m:ss> to move backward. Examples: skip 35, skp -10".to_string());
        return Ok(());
    }

    let Some(engine) = app.playback.as_ref().cloned() else {
        push_playback_unavailable(app);
        return Ok(());
    };

    engine.tick();
    let snapshot = engine.snapshot();
    if matches!(snapshot.state, PlaybackState::Stopped | PlaybackState::Error) || snapshot.current.is_none() {
        app.push_message("Nothing is currently loaded to skip.".to_string());
        return Ok(());
    }

    let relative_arg = relative_seek_argument(trimmed, '+');
    let target_ms = parse_seek_target_ms(&relative_arg, snapshot.position_ms, snapshot.duration_ms)?;
    engine.seek(Duration::from_millis(target_ms))?;
    let duration = snapshot
        .duration_ms
        .map(format_ms)
        .unwrap_or_else(|| "--:--".to_string());
    let direction = if relative_arg.starts_with('-') { "back" } else { "forward" };
    app.push_message(format!("Skipped {} to {}/{}.", direction, format_ms(target_ms), duration));
    Ok(())
}


fn relative_seek_argument(value: &str, default_sign: char) -> String {
    let trimmed = value.trim();
    if trimmed.starts_with('+') || trimmed.starts_with('-') {
        trimmed.to_string()
    } else {
        format!("{}{}", default_sign, trimmed)
    }
}

fn parse_seek_target_ms(value: &str, current_ms: u64, duration_ms: Option<u64>) -> Result<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("Seek value is required."));
    }

    let (relative, magnitude) = if let Some(rest) = trimmed.strip_prefix('+') {
        (Some(1i128), rest.trim())
    } else if let Some(rest) = trimmed.strip_prefix('-') {
        (Some(-1i128), rest.trim())
    } else {
        (None, trimmed)
    };

    let amount_ms = parse_time_value_ms(magnitude)? as i128;
    let raw_target = match relative {
        Some(sign) => (current_ms as i128) + sign * amount_ms,
        None => amount_ms,
    };

    let mut target = raw_target.max(0) as u64;
    if let Some(duration) = duration_ms {
        target = target.min(duration);
    }
    Ok(target)
}

fn parse_time_value_ms(value: &str) -> Result<u64> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("Time value is required."));
    }

    if let Some(seconds) = trimmed.strip_suffix('s') {
        let seconds = seconds
            .trim()
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("Seconds value must be a positive number."))?;
        return Ok(seconds * 1000);
    }

    if let Some(minutes) = trimmed.strip_suffix('m') {
        let minutes = minutes
            .trim()
            .parse::<u64>()
            .map_err(|_| anyhow::anyhow!("Minutes value must be a positive number."))?;
        return Ok(minutes * 60 * 1000);
    }

    if trimmed.contains(':') {
        let parts: Vec<&str> = trimmed.split(':').collect();
        if parts.len() < 2 || parts.len() > 3 {
            return Err(anyhow::anyhow!("Use m:ss or h:mm:ss for seek times."));
        }
        let mut total_seconds = 0u64;
        for part in parts {
            let n = part
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("Seek time contains a non-numeric component."))?;
            total_seconds = total_seconds * 60 + n;
        }
        return Ok(total_seconds * 1000);
    }

    let seconds = trimmed
        .parse::<u64>()
        .map_err(|_| anyhow::anyhow!("Use seconds, m:ss, h:mm:ss, or +/- relative seek values."))?;
    Ok(seconds * 1000)
}

fn playback_snapshot_matches_queue_selection(app: &AppState, current: Option<&PlaybackTrack>) -> bool {
    let Some(current_track) = current else {
        return false;
    };
    let Some(index) = app.current_queue_index else {
        return false;
    };
    let Some(queue_track) = app.queue.get(index) else {
        return false;
    };
    current_track.id == queue_track.id && current_track.server_alias == queue_track.server_alias
}

fn queue_index_for_playback_track(app: &AppState, current_track: &PlaybackTrack) -> Option<usize> {
    app.queue.iter().position(|queue_track| {
        queue_track.id == current_track.id && queue_track.server_alias == current_track.server_alias
    })
}

fn schedule_queue_playback_after_grace(app: &mut AppState, index: usize, refresh_queue_view: bool) -> Result<()> {
    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return Ok(());
    }

    if app.queue.get(index).is_none() {
        app.push_message(format!("No queue item at number {}", index + 1));
        return Ok(());
    }

    app.current_queue_index = Some(index);
    app.ensure_queue_index_visible(index);
    app.paused_selection_pending = false;
    app.pending_queue_playback = Some(PendingQueuePlayback {
        index,
        due_at: Instant::now() + Duration::from_millis(QUEUE_SELECTION_PLAYBACK_GRACE_MS),
    });
    if let Some(engine) = app.playback.as_ref() {
        engine.invalidate_prepared_next();
    }
    refresh_queue_view_if_active(app, refresh_queue_view);
    Ok(())
}

fn handle_deferred_queue_playback(app: &mut AppState) -> Result<()> {
    let Some(pending) = app.pending_queue_playback else {
        return Ok(());
    };

    if Instant::now() < pending.due_at {
        return Ok(());
    }

    app.pending_queue_playback = None;
    if app.queue.is_empty() || pending.index >= app.queue.len() {
        return Ok(());
    }
    if app.current_queue_index != Some(pending.index) {
        return Ok(());
    }

    let playback_state = app
        .playback
        .as_ref()
        .map(|engine| engine.snapshot().state)
        .unwrap_or(PlaybackState::Stopped);

    if matches!(playback_state, PlaybackState::Playing | PlaybackState::Buffering) {
        play_queue_index(app, pending.index)?;
    }
    Ok(())
}

fn select_queue_index(app: &mut AppState, index: usize) -> Result<()> {
    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return Ok(());
    }

    let refresh_queue_view = matches!(app.selection_context, Some(SelectionContext::Queue));

    let track = app
        .queue
        .get(index)
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("No queue item at number {}", index + 1))?;

    let playback_state = app
        .playback
        .as_ref()
        .map(|engine| engine.snapshot().state)
        .unwrap_or(PlaybackState::Stopped);

    match playback_state {
        PlaybackState::Playing | PlaybackState::Buffering => {
            schedule_queue_playback_after_grace(app, index, refresh_queue_view)?;
        }
        PlaybackState::Paused => {
            app.current_queue_index = Some(index);
            app.ensure_queue_index_visible(index);
            app.enter_queue_context();
            app.paused_selection_pending = true;
            app.push_message(format!(
                "Selected queue item {}: {}. Playback remains paused; press Space or use 'play' to start it.",
                index + 1,
                app.track_label(&track)
            ));
            refresh_queue_view_if_active(app, refresh_queue_view);
        }
        PlaybackState::Stopped | PlaybackState::Error => {
            app.current_queue_index = Some(index);
            app.ensure_queue_index_visible(index);
            app.enter_queue_context();
            app.paused_selection_pending = false;
            app.push_message(format!(
                "Selected queue item {}: {}. Use 'play' to start it.",
                index + 1,
                app.track_label(&track)
            ));
            refresh_queue_view_if_active(app, refresh_queue_view);
        }
    }

    Ok(())
}


fn show_now_playing(app: &mut AppState) {
    let queue_position = app
        .current_queue_index
        .map(|idx| format!("{}/{}", idx + 1, app.queue.len()))
        .unwrap_or_else(|| "none".to_string());
    let selected = app
        .current_queue_index
        .and_then(|idx| app.queue.get(idx))
        .map(|track| app.track_label(track))
        .unwrap_or_else(|| "none".to_string());

    match app.playback.as_ref().cloned() {
        Some(engine) => {
            engine.tick();
            let snapshot = engine.snapshot();
            let state = match snapshot.state {
                PlaybackState::Stopped => "stopped",
                PlaybackState::Playing => "playing",
                PlaybackState::Paused => "paused",
                PlaybackState::Buffering => "buffering",
                PlaybackState::Error => "error",
            };
            let active = snapshot
                .current
                .as_ref()
                .map(|track| app.playback_track_label(track))
                .unwrap_or_else(|| "none".to_string());
            let duration = snapshot
                .duration_ms
                .map(format_ms)
                .unwrap_or_else(|| "--:--".to_string());
            app.push_message(format!(
                "Now: {} | Repeat: {} | Shuffle: {} | Shuffle order: {} | Queue: {} | Selected: {} | Playing: {} | Vol: {} | Pos: {}/{} | Audio: {}",
                state,
                app.repeat_mode.label(),
                on_off(app.shuffle_enabled),
                shuffle_order_status_label(app),
                queue_position,
                selected,
                active,
                snapshot.volume,
                format_ms(snapshot.position_ms),
                duration,
                snapshot.output_device.as_deref().unwrap_or("unknown")
            ));
        }
        None => {
            app.push_message(format!(
                "Now: playback unavailable ({}) | Queue: {} | Selected: {}",
                app.playback_init_error
                    .as_deref()
                    .unwrap_or("audio output could not be initialised"),
                queue_position,
                selected
            ));
        }
    }
}

fn expand_compact_single_letter_command(command: &str) -> String {
    let trimmed = command.trim();
    let mut chars = trimmed.chars();
    let Some(prefix) = chars.next() else {
        return String::new();
    };
    let rest = chars.as_str().trim_start();

    match prefix {
        'a' if looks_like_add_args(rest) => format!("a {}", rest),
        'p' if rest == "*" || looks_like_add_args(rest) => format!("p {}", rest),
        'x' if looks_like_positive_number(rest) => format!("x {}", rest),
        'r' if looks_like_queue_number_args(rest) => format!("r {}", rest),
        _ => trimmed.to_string(),
    }
}

fn looks_like_queue_number_args(value: &str) -> bool {
    looks_like_number_selector_args(value, false)
}

fn parse_queue_number_list(args: &str, command_name: &str) -> Result<Vec<usize>> {
    parse_number_selector_list(args, command_name)
}

fn parse_number_selector_list(args: &str, command_name: &str) -> Result<Vec<usize>> {
    let mut numbers = Vec::new();
    for token in args.replace(',', " ").split_whitespace() {
        let expanded = expand_number_selector_token(token, command_name)?;
        for index in expanded {
            if !numbers.contains(&index) {
                numbers.push(index);
            }
        }
    }
    Ok(numbers)
}

fn expand_number_selector_token(token: &str, command_name: &str) -> Result<Vec<usize>> {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return Ok(Vec::new());
    }

    if let Some((start, end)) = parse_number_range_token(trimmed, command_name)? {
        let iter: Box<dyn Iterator<Item = usize>> = if start <= end {
            Box::new(start..=end)
        } else {
            Box::new((end..=start).rev())
        };
        return Ok(iter.map(|number| number - 1).collect());
    }

    let number = trimmed
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid number selector '{}' for {}. Use numbers, comma-separated lists, or ranges like 1-5.", trimmed, command_name))?;
    if number == 0 {
        return Err(anyhow::anyhow!("Numbers are 1-based."));
    }
    Ok(vec![number - 1])
}

fn parse_number_range_token(token: &str, command_name: &str) -> Result<Option<(usize, usize)>> {
    let separator = if token.contains("..") {
        Some("..")
    } else if token.matches('-').count() == 1 && !token.starts_with('-') && !token.ends_with('-') {
        Some("-")
    } else {
        None
    };

    let Some(separator) = separator else {
        return Ok(None);
    };

    let mut parts = token.split(separator);
    let start_text = parts.next().unwrap_or_default().trim();
    let end_text = parts.next().unwrap_or_default().trim();
    if parts.next().is_some() || start_text.is_empty() || end_text.is_empty() {
        return Err(anyhow::anyhow!("Invalid range '{}' for {}. Use a range like 1-5 or 1..5.", token, command_name));
    }

    let start = start_text
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid range start '{}' for {}.", start_text, command_name))?;
    let end = end_text
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Invalid range end '{}' for {}.", end_text, command_name))?;
    if start == 0 || end == 0 {
        return Err(anyhow::anyhow!("Range numbers are 1-based."));
    }
    Ok(Some((start, end)))
}

fn looks_like_number_selector_args(value: &str, allow_star: bool) -> bool {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return false;
    }
    if allow_star && trimmed == "*" {
        return true;
    }
    let mut saw_selector = false;
    for token in trimmed.replace(',', " ").split_whitespace() {
        if token == "*" {
            return allow_star;
        }
        if !looks_like_number_selector_token(token) {
            return false;
        }
        saw_selector = true;
    }
    saw_selector
}

fn looks_like_number_selector_token(token: &str) -> bool {
    let trimmed = token.trim();
    if trimmed.is_empty() {
        return false;
    }
    if looks_like_positive_number(trimmed) {
        return true;
    }
    if let Some((left, right)) = trimmed.split_once("..") {
        return looks_like_positive_number(left) && looks_like_positive_number(right);
    }
    if trimmed.matches('-').count() == 1 && !trimmed.starts_with('-') && !trimmed.ends_with('-') {
        let mut parts = trimmed.split('-');
        return parts
            .next()
            .map(looks_like_positive_number)
            .unwrap_or(false)
            && parts
                .next()
                .map(looks_like_positive_number)
                .unwrap_or(false)
            && parts.next().is_none();
    }
    false
}



fn sanitized_queue_file_stem(name: &str) -> Result<String> {
    let mut out = String::new();
    let mut last_sep = false;

    for ch in name.trim().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_sep = false;
        } else if ch == '-' || ch == '_' || ch.is_whitespace() {
            if !last_sep && !out.is_empty() {
                out.push('_');
                last_sep = true;
            }
        }
    }

    while out.ends_with('_') {
        out.pop();
    }

    if out.is_empty() {
        Err(anyhow::anyhow!("Saved queue name must contain at least one letter or number."))
    } else {
        Ok(out)
    }
}

fn saved_queue_path(app: &AppState, name: &str) -> Result<PathBuf> {
    let stem = sanitized_queue_file_stem(name)?;
    Ok(app.store.queue_dir().join(format!("{}.toml", stem)))
}

fn ensure_saved_queue_dir(app: &AppState) -> Result<PathBuf> {
    let dir = app.store.queue_dir();
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

fn suggested_queue_name(app: &AppState) -> String {
    if app.queue.is_empty() {
        return "queue".to_string();
    }

    if app.queue.len() == 1 {
        let track = &app.queue[0];
        return format!("{} - {}", track.artist, track.title);
    }

    let first = &app.queue[0];
    let same_album = !first.album.trim().is_empty()
        && app.queue.iter().all(|track| track.album == first.album && track.artist == first.artist);
    if same_album {
        return format!("{} - {}", first.artist, first.album);
    }

    let same_artist = !first.artist.trim().is_empty()
        && app.queue.iter().all(|track| track.artist == first.artist);
    if same_artist {
        return format!("{} queue", first.artist);
    }

    let seconds = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format!("queue {} tracks {}", app.queue.len(), seconds)
}

fn show_about(app: &mut AppState) {
    app.push_message(format!("DISC {} release candidate (package {}).", BUILD_LABEL, env!("CARGO_PKG_VERSION")));
    app.push_message(format!("Config file: {}", app.store.path().display()));
    app.push_message(format!("Primary server: {}", app.config.primary_display_name()));
    app.push_message(format!("Servers configured: {} | Queue: {} track(s) | Repeat: {} | Shuffle: {} | Shuffle order: {} | Gapless: {} | Media keys: {} | Queue-follow: {} | Messages: {}", app.config.servers.len(), app.queue.len(), app.repeat_mode.label(), on_off(app.shuffle_enabled), shuffle_order_status_label(app), on_off(app.config.gapless_playback), on_off(app.config.media_keys_enabled), on_off(app.config.queue_follow), on_off(app.config.messages_visible)));
    app.push_message("Use help, help browse, help queue, help playback, help saved, help session, help doctor, or help style.".to_string());
}

fn persist_last_session(app: &AppState, _announce: bool) -> Result<()> {
    let path = app.store.session_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let saved = SavedQueue {
        name: LAST_SESSION_NAME.to_string(),
        current_index: app.current_queue_index,
        tracks: app.queue.clone(),
    };
    let text = toml::to_string_pretty(&saved)?;
    fs::write(&path, text)?;
    Ok(())
}

fn handle_session_status_command(app: &mut AppState) -> Result<()> {
    let path = app.store.session_path();
    if !path.exists() {
        app.push_message("No last-session file yet. The current queue is saved automatically when you quit, or use session save.".to_string());
        return Ok(());
    }
    let saved = read_saved_queue(&path)?;
    if saved.tracks.is_empty() {
        app.push_message("Last session exists but the saved queue is empty.".to_string());
    } else {
        let selected = saved.current_index.map(|idx| idx + 1).unwrap_or(1);
        app.push_message(format!("Last session: {} track(s), saved position {}. Use restore-session to load it.", saved.tracks.len(), selected));
    }
    Ok(())
}

fn handle_restore_session_command(app: &mut AppState) -> Result<()> {
    let path = app.store.session_path();
    if !path.exists() {
        app.push_message("No last session is available yet. Quit once with a queue, or use session save.".to_string());
        return Ok(());
    }
    let mut saved = read_saved_queue(&path)?;
    saved.name = LAST_SESSION_NAME.to_string();
    if saved.tracks.is_empty() {
        app.push_message("Last session queue is empty.".to_string());
        return Ok(());
    }
    replace_with_saved_queue(app, saved)
}

fn handle_clear_session_command(app: &mut AppState) -> Result<()> {
    let path = app.store.session_path();
    if path.exists() {
        fs::remove_file(&path)?;
        app.push_message("Last session cleared.".to_string());
    } else {
        app.push_message("No last session file to clear.".to_string());
    }
    Ok(())
}

fn handle_save_queue_command(app: &mut AppState, args: &str) -> Result<()> {
    if app.queue.is_empty() {
        app.push_message("Queue is empty; nothing to save.".to_string());
        return Ok(());
    }

    let (requested_name, replace_existing) = parse_save_queue_args(args);
    let owned_name = if requested_name.trim().is_empty() {
        let suggested = next_available_saved_queue_name(app, &suggested_queue_name(app))?;
        app.push_message(format!("No saved-queue name supplied; using '{}'.", suggested));
        suggested
    } else {
        requested_name.trim().to_string()
    };
    let name = owned_name.as_str();

    ensure_saved_queue_dir(app)?;
    let path = saved_queue_path(app, name)?;
    if path.exists() && !replace_existing {
        app.push_message(format!(
            "Saved queue '{}' already exists. Use 'save-queue {} --replace' to overwrite it, or choose a different name.",
            name,
            name
        ));
        return Ok(());
    }

    let saved = SavedQueue {
        name: name.to_string(),
        current_index: app.current_queue_index,
        tracks: app.queue.clone(),
    };
    let text = toml::to_string_pretty(&saved)?;
    fs::write(&path, text)?;
    app.queue_playlist_name = Some(saved.name.clone());
    app.push_message(format!(
        "{} saved queue '{}' with {} track(s). Use 'load-queue {}' to restore it.",
        if replace_existing { "Replaced" } else { "Saved" },
        saved.name,
        saved.tracks.len(),
        saved.name
    ));
    refresh_saved_queue_list_if_active(app)?;
    Ok(())
}

fn parse_save_queue_args(args: &str) -> (String, bool) {
    let mut replace_existing = false;
    let mut parts = Vec::new();
    for token in args.split_whitespace() {
        if flag_is(token, &["replace", "overwrite", "force", "r", "f"]) {
            replace_existing = true;
        } else {
            parts.push(token);
        }
    }
    (parts.join(" "), replace_existing)
}

fn next_available_saved_queue_name(app: &AppState, base_name: &str) -> Result<String> {
    let base = base_name.trim();
    if base.is_empty() {
        return Ok("queue".to_string());
    }
    if !saved_queue_path(app, base)?.exists() {
        return Ok(base.to_string());
    }
    for suffix in 2..1000 {
        let candidate = format!("{} {}", base, suffix);
        if !saved_queue_path(app, &candidate)?.exists() {
            return Ok(candidate);
        }
    }
    Ok(format!("{} {}", base, SystemTime::now().duration_since(UNIX_EPOCH).map(|duration| duration.as_secs()).unwrap_or(0)))
}

fn saved_queue_sources(saved: &SavedQueue) -> Vec<String> {
    let mut sources = Vec::new();
    for track in &saved.tracks {
        if !sources.iter().any(|source: &String| source.eq_ignore_ascii_case(&track.server_alias)) {
            sources.push(track.server_alias.clone());
        }
    }
    sources
}

fn saved_queue_track_label(track: &QueueTrack) -> String {
    format!("{} — {}", track.title, track.artist)
}

fn refresh_saved_queue_list_if_active(app: &mut AppState) -> Result<()> {
    if matches!(app.selection_context, Some(SelectionContext::SavedQueues)) {
        let entries = scan_saved_queues(app)?;
        if entries.is_empty() {
            app.saved_queues = None;
        } else {
            app.set_saved_queues(SavedQueueListState::new(entries));
        }
    }
    Ok(())
}

fn read_saved_queue(path: &PathBuf) -> Result<SavedQueue> {
    let text = fs::read_to_string(path)?;
    let saved: SavedQueue = toml::from_str(&text)?;
    Ok(saved)
}

fn scan_saved_queues(app: &AppState) -> Result<Vec<SavedQueueListEntry>> {
    let dir = app.store.queue_dir();
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut entries = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        match read_saved_queue(&path) {
            Ok(saved) => {
                let sources = saved_queue_sources(&saved);
                let first_track_label = saved.tracks.first().map(saved_queue_track_label);
                entries.push(SavedQueueListEntry {
                    name: saved.name,
                    track_count: saved.tracks.len(),
                    current_index: saved.current_index,
                    sources,
                    first_track_label,
                    path,
                    unreadable: false,
                });
            }
            Err(_) => {
                let fallback = path
                    .file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or("unreadable")
                    .to_string();
                entries.push(SavedQueueListEntry {
                    name: format!("{} (unreadable)", fallback),
                    track_count: 0,
                    current_index: None,
                    sources: Vec::new(),
                    first_track_label: None,
                    path,
                    unreadable: true,
                });
            }
        }
    }

    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(entries)
}

fn handle_list_saved_queues(app: &mut AppState) -> Result<()> {
    let entries = scan_saved_queues(app)?;
    if entries.is_empty() {
        app.saved_queues = None;
        app.push_message("No saved queues.".to_string());
        return Ok(());
    }

    app.set_saved_queues(SavedQueueListState::new(entries));
    Ok(())
}

fn resolve_saved_queue_reference(app: &AppState, reference: &str, command_name: &str) -> Result<SavedQueueListEntry> {
    let reference = reference.trim();
    if reference.is_empty() {
        return Err(anyhow::anyhow!("Usage: {} <saved-queue-name-or-number>", command_name));
    }

    if reference.chars().all(|ch| ch.is_ascii_digit()) {
        let number = reference
            .parse::<usize>()
            .map_err(|_| anyhow::anyhow!("{} needs a saved queue number or name.", command_name))?;
        if number == 0 {
            return Err(anyhow::anyhow!("Saved queue numbers are 1-based."));
        }
        let entry = app
            .resolve_saved_queue_index(number - 1)
            .ok_or_else(|| {
                let count = app
                    .saved_queues
                    .as_ref()
                    .map(|state| state.entries.len())
                    .unwrap_or(0);
                if count == 0 {
                    anyhow::anyhow!("No saved queue list is active. Use 'queues' first, then {} {}.", command_name, number)
                } else {
                    anyhow::anyhow!("No saved queue at number {}. Current saved queue list contains {} item(s).", number, count)
                }
            })?;
        return Ok(entry);
    }

    let path = saved_queue_path(app, reference)?;
    Ok(SavedQueueListEntry {
        name: reference.to_string(),
        track_count: 0,
        current_index: None,
        sources: Vec::new(),
        first_track_label: None,
        path,
        unreadable: false,
    })
}

fn load_saved_queue_entry(entry: &SavedQueueListEntry) -> Result<SavedQueue> {
    if entry.unreadable {
        return Err(anyhow::anyhow!("Saved queue '{}' could not be read.", entry.name));
    }
    read_saved_queue(&entry.path)
}

fn replace_with_saved_queue(app: &mut AppState, saved: SavedQueue) -> Result<()> {
    if saved.tracks.is_empty() {
        app.push_message(format!("Saved queue '{}' is empty.", saved.name));
        return Ok(());
    }

    let saved_name = saved.name.clone();
    app.queue = saved.tracks;
    app.queue_playlist_name = if saved_name == LAST_SESSION_NAME { None } else { Some(saved_name.clone()) };
    app.shuffle_original_order = None;
    app.reset_shuffle_play_order();
    let selected = saved
        .current_index
        .filter(|idx| *idx < app.queue.len())
        .unwrap_or(0);
    app.current_queue_index = Some(selected);
    app.queue_page_start = 0;
    app.ensure_queue_index_visible(selected);
    app.enter_queue_context();
    app.paused_selection_pending = false;

    let label = app
        .queue
        .get(selected)
        .map(|track| app.track_label(track))
        .unwrap_or_else(|| "none".to_string());
    app.push_message(format!(
        "Loaded queue '{}' with {} track(s). Current: {}",
        saved_name,
        app.queue.len(),
        label
    ));
    autoplay_after_queue_change(app, true, true)
}

fn append_saved_queue(app: &mut AppState, saved: SavedQueue) -> Result<()> {
    if saved.tracks.is_empty() {
        app.push_message(format!("Saved queue '{}' is empty; nothing to add.", saved.name));
        return Ok(());
    }

    let was_empty = app.queue.is_empty();
    let name = saved.name;
    let count = saved.tracks.len();
    app.append_queue(saved.tracks);
    if was_empty {
        app.queue_playlist_name = Some(name.clone());
    }
    app.push_message(format!("Added saved queue '{}' ({} track(s)) to the play queue.", name, count));
    autoplay_after_queue_change(app, was_empty, false)
}

fn handle_saved_queue_numeric_selection(app: &mut AppState, index: usize) -> Result<()> {
    let entry = app
        .resolve_saved_queue_index(index)
        .ok_or_else(|| {
            let count = app
                .saved_queues
                .as_ref()
                .map(|state| state.entries.len())
                .unwrap_or(0);
            anyhow::anyhow!("No saved queue at number {}. Current saved queue list contains {} item(s).", index + 1, count)
        })?;
    let saved = load_saved_queue_entry(&entry)?;
    replace_with_saved_queue(app, saved)
}

fn handle_load_queue_command(app: &mut AppState, reference: &str) -> Result<()> {
    let reference = reference.trim();
    if reference.is_empty() {
        app.push_message("Usage: load-queue <name-or-number>".to_string());
        return Ok(());
    }

    let entry = resolve_saved_queue_reference(app, reference, "load-queue")?;
    if !entry.path.exists() {
        app.push_message(format!("No saved queue named '{}'. Use 'queues' to list saved queues.", reference));
        return Ok(());
    }

    let saved = load_saved_queue_entry(&entry)?;
    replace_with_saved_queue(app, saved)
}

fn handle_add_saved_queue_command(app: &mut AppState, args: &str) -> Result<()> {
    let args = args.trim();
    if args.is_empty() {
        app.push_message("Usage: a <saved-queue-number...>".to_string());
        return Ok(());
    }
    if args == "*" {
        let entries = app
            .saved_queues
            .as_ref()
            .map(|state| state.entries.clone())
            .unwrap_or_default();
        if entries.is_empty() {
            app.push_message("No saved queue list is active. Use 'queues' first.".to_string());
            return Ok(());
        }
        for entry in entries {
            let saved = load_saved_queue_entry(&entry)?;
            append_saved_queue(app, saved)?;
        }
        return Ok(());
    }

    let references: Vec<String> = if looks_like_number_selector_args(args, false) {
        parse_number_selector_list(args, "a")?
            .into_iter()
            .map(|idx| (idx + 1).to_string())
            .collect()
    } else {
        args.replace(',', " ")
            .split_whitespace()
            .map(|token| token.to_string())
            .collect()
    };

    for reference in references {
        let entry = resolve_saved_queue_reference(app, &reference, "a")?;
        if !entry.path.exists() {
            app.push_message(format!("No saved queue named '{}'. Use 'queues' to list saved queues.", reference));
            continue;
        }
        let saved = load_saved_queue_entry(&entry)?;
        append_saved_queue(app, saved)?;
    }
    Ok(())
}

fn handle_rename_queue_command(app: &mut AppState, args: &str) -> Result<()> {
    let (args_without_flags, replace_existing) = parse_save_queue_args(args);
    let mut parts = args_without_flags.split_whitespace();
    let reference = match parts.next() {
        Some(value) => value,
        None => {
            app.push_message("Usage: rename-queue <number-or-name> <new-name> [--replace]".to_string());
            return Ok(());
        }
    };
    let new_name = parts.collect::<Vec<_>>().join(" ");
    if new_name.trim().is_empty() {
        app.push_message("Usage: rename-queue <number-or-name> <new-name> [--replace]".to_string());
        return Ok(());
    }

    let entry = resolve_saved_queue_reference(app, reference, "rename-queue")?;
    if !entry.path.exists() {
        app.push_message(format!("No saved queue named '{}'. Use 'queues' to list saved queues.", reference));
        return Ok(());
    }

    let mut saved = load_saved_queue_entry(&entry)?;
    let new_path = saved_queue_path(app, &new_name)?;
    if new_path == entry.path {
        saved.name = new_name.trim().to_string();
        let text = toml::to_string_pretty(&saved)?;
        fs::write(&entry.path, text)?;
        app.push_message(format!("Saved queue name updated to '{}'.", saved.name));
        refresh_saved_queue_list_if_active(app)?;
        return Ok(());
    }

    if new_path.exists() && !replace_existing {
        app.push_message(format!(
            "A saved queue named '{}' already exists. Use 'rename-queue {} {} --replace' to overwrite it.",
            new_name.trim(),
            reference,
            new_name.trim()
        ));
        return Ok(());
    }

    saved.name = new_name.trim().to_string();
    let text = toml::to_string_pretty(&saved)?;
    fs::write(&new_path, text)?;
    fs::remove_file(&entry.path)?;
    app.push_message(format!("Renamed saved queue '{}' to '{}'.", entry.name, saved.name));
    refresh_saved_queue_list_if_active(app)?;
    Ok(())
}

fn handle_delete_queue_command(app: &mut AppState, reference: &str) -> Result<()> {
    let reference = reference.trim();
    if reference.is_empty() {
        app.push_message("Usage: delete-queue <name-or-number>".to_string());
        return Ok(());
    }

    let entry = resolve_saved_queue_reference(app, reference, "delete-queue")?;
    if !entry.path.exists() {
        app.push_message(format!("No saved queue named '{}'.", reference));
        return Ok(());
    }
    fs::remove_file(&entry.path)?;
    app.push_message(format!("Deleted saved queue '{}'.", entry.name));

    if matches!(app.selection_context, Some(SelectionContext::SavedQueues)) {
        let entries = scan_saved_queues(app)?;
        if entries.is_empty() {
            app.saved_queues = None;
            app.push_message("No saved queues.".to_string());
        } else {
            app.set_saved_queues(SavedQueueListState::new(entries));
        }
    }
    Ok(())
}

fn handle_repeat_command(app: &mut AppState, value: &str) {
    let lower = value.trim().to_lowercase();
    let next = match lower.as_str() {
        "off" | "none" | "0" => Some(RepeatMode::Off),
        "one" | "track" | "1" => Some(RepeatMode::One),
        "all" | "queue" | "q" => Some(RepeatMode::All),
        _ => None,
    };

    match next {
        Some(mode) => {
            app.repeat_mode = mode;
            app.push_message(format!("Repeat mode set to {}.", app.repeat_mode.label()));
        }
        None => app.push_message("Usage: repeat off | repeat one | repeat all".to_string()),
    }
}

fn toggle_shuffle_mode(app: &mut AppState) {
    let next = !app.shuffle_enabled;
    set_shuffle_mode(app, next);
}

fn handle_shuffle_mode_or_reorder_command(app: &mut AppState, value: &str) {
    let lower = value.trim().to_lowercase();
    match lower.as_str() {
        "on" => set_shuffle_mode(app, true),
        "off" => set_shuffle_mode(app, false),
        "toggle" => toggle_shuffle_mode(app),
        "status" => app.push_message(format!(
            "Shuffle mode: {}; shuffle order: {}.",
            on_off(app.shuffle_enabled),
            shuffle_order_status_label(app)
        )),
        "all" => handle_shuffle_reorder_command(app, true),
        "queue" | "upcoming" | "now" => handle_shuffle_reorder_command(app, false),
        _ => app.push_message("Usage: shuffle/sh [on|off|toggle|status|all|upcoming]. Bare shuffle or sh toggles playback shuffle; so controls stable visible shuffle order; unshuffle/unsh restores original order and turns shuffle off.".to_string()),
    }
}

fn set_shuffle_mode(app: &mut AppState, enabled: bool) {
    let was_enabled = app.shuffle_enabled;
    app.shuffle_enabled = enabled;
    if !enabled || app.shuffle_order_enabled {
        app.reset_shuffle_play_order();
    }

    if app.shuffle_enabled && app.shuffle_order_enabled {
        let changed = stable_shuffle_queue_order(app, true);
        if changed > 0 {
            app.enter_queue_context();
            app.push_message(format!(
                "Shuffle mode on. Stable shuffle order applied to {} queue item(s).",
                changed
            ));
        } else if app.queue.len() < 2 {
            app.push_message("Shuffle mode on. Shuffle order is on, but the queue has fewer than two item(s).".to_string());
        } else if !was_enabled {
            app.push_message("Shuffle mode on. Shuffle order is on; no queue item(s) to reorder.".to_string());
        } else {
            app.push_message("Shuffle mode on. Shuffle order is on.".to_string());
        }
    } else if app.shuffle_enabled {
        rebuild_random_shuffle_play_order_from_current(app);
        app.push_message("Shuffle mode on. Queue order will stay fixed; next/previous will move through a random playback path. Use 'so' for a stable visible shuffled queue order.".to_string());
    } else {
        let restored = if app.shuffle_order_enabled {
            restore_original_queue_order(app)
        } else {
            0
        };
        if restored > 0 {
            app.enter_queue_context();
            app.push_message(format!(
                "Shuffle mode off. Restored original queue order for {} item(s). Shuffle order is {}.",
                restored,
                shuffle_order_status_label(app)
            ));
        } else {
            app.push_message(format!(
                "Shuffle mode off. Shuffle order is {}.",
                shuffle_order_status_label(app)
            ));
        }
    }
}

fn toggle_shuffle_order_mode(app: &mut AppState) {
    set_shuffle_order_mode(app, !app.shuffle_order_enabled);
}

fn handle_shuffle_order_command(app: &mut AppState, value: &str) {
    let lower = value.trim().to_lowercase();
    match lower.as_str() {
        "on" => set_shuffle_order_mode(app, true),
        "off" => set_shuffle_order_mode(app, false),
        "toggle" | "" => toggle_shuffle_order_mode(app),
        "status" => app.push_message(format!("Shuffle order: {}.", shuffle_order_status_label(app))),
        "restore" | "unshuffle" | "unsh" => restore_visible_queue_order_command(app),
        "refresh" | "reshuffle" | "now" => {
            if !app.shuffle_order_enabled {
                app.push_message("Shuffle order is off. Use 'so on' before reshuffling the stable order.".to_string());
            } else if !app.shuffle_enabled {
                app.push_message("Shuffle order is standby because shuffle mode is off. Use 'sh' to activate it.".to_string());
            } else {
                let changed = stable_shuffle_queue_order(app, true);
                if changed > 0 {
                    app.enter_queue_context();
                    app.push_message(format!("Refreshed stable shuffle order for {} queue item(s).", changed));
                } else {
                    app.push_message("No upcoming queue items to reshuffle from the current position.".to_string());
                }
            }
        }
        _ => app.push_message("Usage: so [on|off|toggle|status|refresh|restore] or shuffle-order [on|off|restore]. Shuffle order is standby when enabled while shuffle mode is off.".to_string()),
    }
}

fn set_shuffle_order_mode(app: &mut AppState, enabled: bool) {
    app.shuffle_order_enabled = enabled;
    app.reset_shuffle_play_order();
    if !enabled {
        let restored = restore_original_queue_order(app);
        if restored > 0 {
            app.enter_queue_context();
            app.push_message(format!(
                "Shuffle order off. Restored original queue order for {} item(s). Shuffle mode can still randomise cursor movement without rearranging the visible queue.",
                restored
            ));
        } else {
            app.push_message("Shuffle order off. Shuffle mode can still randomise playback cursor movement without rearranging the visible queue.".to_string());
        }
        return;
    }

    if app.shuffle_enabled {
        let changed = stable_shuffle_queue_order(app, true);
        if changed > 0 {
            app.enter_queue_context();
            app.push_message(format!(
                "Shuffle order on. Reordered {} queue item(s) into a stable shuffled order.",
                changed
            ));
        } else if app.queue.len() < 2 {
            app.push_message("Shuffle order on. Queue has fewer than two item(s), so there is nothing to reorder.".to_string());
        } else {
            app.push_message("Shuffle order on. No queue items to reorder.".to_string());
        }
    } else {
        app.push_message("Shuffle order standby. Turn shuffle on with 'sh' to apply a stable shuffled play order.".to_string());
    }
}

fn remember_original_queue_order(app: &mut AppState) {
    if app.shuffle_original_order.is_none() && !app.queue.is_empty() {
        app.shuffle_original_order = Some(app.queue.clone());
    }
}

fn restore_original_queue_order(app: &mut AppState) -> usize {
    let Some(original_order) = app.shuffle_original_order.take() else {
        return 0;
    };
    if original_order.is_empty() || app.queue.is_empty() {
        return 0;
    }

    let current_key = app
        .current_queue_index
        .and_then(|idx| app.queue.get(idx))
        .map(queue_track_key);

    let mut remaining: HashMap<String, usize> = HashMap::new();
    for track in &app.queue {
        *remaining.entry(queue_track_key(track)).or_insert(0) += 1;
    }

    let mut restored = Vec::with_capacity(app.queue.len());
    for track in original_order {
        let key = queue_track_key(&track);
        if let Some(count) = remaining.get_mut(&key) {
            if *count > 0 {
                restored.push(track);
                *count -= 1;
            }
        }
    }

    for track in app.queue.iter().cloned() {
        let key = queue_track_key(&track);
        if let Some(count) = remaining.get_mut(&key) {
            if *count > 0 {
                restored.push(track);
                *count -= 1;
            }
        }
    }

    if restored.is_empty() {
        return 0;
    }

    let restored_len = restored.len();
    app.queue = restored;
    app.reset_shuffle_play_order();
    if let Some(key) = current_key {
        app.current_queue_index = app.queue.iter().position(|track| queue_track_key(track) == key);
    }
    if app.current_queue_index.is_none() && !app.queue.is_empty() {
        app.current_queue_index = Some(0);
    }
    if let Some(index) = app.current_queue_index {
        app.ensure_queue_index_visible(index);
    }
    queue_playback_plan_changed(app);
    restored_len
}

fn restore_visible_queue_order_command(app: &mut AppState) {
    let restored = restore_original_queue_order(app);
    let was_shuffle_enabled = app.shuffle_enabled;
    app.shuffle_enabled = false;
    app.reset_shuffle_play_order();

    if restored > 0 {
        app.enter_queue_context();
        app.push_message(format!(
            "Restored original queue order for {} item(s). Shuffle mode is off; shuffle order is {}.",
            restored,
            shuffle_order_status_label(app)
        ));
    } else if was_shuffle_enabled {
        app.push_message(format!(
            "Shuffle mode is off. No stored shuffled queue order to restore; shuffle order is {}.",
            shuffle_order_status_label(app)
        ));
    } else {
        app.push_message(format!(
            "No stored shuffled queue order to restore. Shuffle mode is off; shuffle order is {}.",
            shuffle_order_status_label(app)
        ));
    }
}

fn stable_shuffle_queue_order(app: &mut AppState, shuffle_all: bool) -> usize {
    app.reset_shuffle_play_order();
    if app.queue.len() < 2 {
        return 0;
    }

    remember_original_queue_order(app);

    let current_key = app
        .current_queue_index
        .and_then(|idx| app.queue.get(idx))
        .map(queue_track_key);

    if shuffle_all || app.current_queue_index.is_none() {
        let before = queue_order_keys(&app.queue);
        let mut rng = rand::thread_rng();
        for _ in 0..6 {
            app.queue.shuffle(&mut rng);
            if queue_order_keys(&app.queue) != before || app.queue.len() < 3 {
                break;
            }
        }
        if let Some(key) = current_key {
            app.current_queue_index = app.queue.iter().position(|track| queue_track_key(track) == key);
        } else {
            app.current_queue_index = Some(0);
        }
        if let Some(index) = app.current_queue_index {
            app.ensure_queue_index_visible(index);
        }
        let changed = if queue_order_keys(&app.queue) == before { 0 } else { app.queue.len() };
        if changed > 0 {
            queue_playback_plan_changed(app);
        }
        return changed;
    }

    let current_index = app.current_queue_index.unwrap_or(0);
    if current_index + 1 >= app.queue.len() {
        return 0;
    }

    let before = queue_order_keys(&app.queue[current_index + 1..]);
    let upcoming_count = app.queue.len() - current_index - 1;
    let mut rng = rand::thread_rng();
    for _ in 0..6 {
        app.queue[current_index + 1..].shuffle(&mut rng);
        if queue_order_keys(&app.queue[current_index + 1..]) != before || upcoming_count < 3 {
            break;
        }
    }
    app.ensure_queue_index_visible(current_index);
    let changed = if queue_order_keys(&app.queue[current_index + 1..]) == before {
        0
    } else {
        upcoming_count
    };
    if changed > 0 {
        queue_playback_plan_changed(app);
    }
    changed
}

fn handle_shuffle_reorder_command(app: &mut AppState, shuffle_all: bool) {
    if app.queue.len() < 2 {
        if app.queue.is_empty() {
            app.push_message("Queue is empty.".to_string());
        } else {
            app.push_message("Queue has only one item; nothing to shuffle.".to_string());
        }
        return;
    }

    let changed = stable_shuffle_queue_order(app, shuffle_all);
    if changed == 0 {
        app.push_message("No upcoming queue items to shuffle. Use 'shuffle all' to shuffle the whole queue.".to_string());
        return;
    }

    app.enter_queue_context();
    if shuffle_all {
        app.push_message("Queue shuffled now. Current selection identity was preserved.".to_string());
    } else {
        let current_index = app.current_queue_index.unwrap_or(0);
        app.push_message(format!(
            "Shuffled {} upcoming queue item(s) after {}. Shuffle mode is {}; shuffle order is {}.",
            changed,
            current_index + 1,
            on_off(app.shuffle_enabled),
            shuffle_order_status_label(app)
        ));
    }
}

fn handle_dedupe_queue_command(app: &mut AppState) {
    let refresh_queue_view = matches!(app.selection_context, Some(SelectionContext::Queue));

    if app.queue.len() < 2 {
        if app.queue.is_empty() {
            app.push_message("Queue is empty.".to_string());
        } else {
            app.push_message("Queue has only one item; nothing to deduplicate.".to_string());
        }
        return;
    }

    let current_index = app.current_queue_index;
    let current_key = current_index
        .and_then(|idx| app.queue.get(idx))
        .map(queue_track_key);

    let original_len = app.queue.len();
    let mut seen: HashSet<String> = HashSet::new();
    let mut deduped = Vec::with_capacity(app.queue.len());

    for (idx, track) in app.queue.iter().cloned().enumerate() {
        let key = queue_track_key(&track);
        if current_key.as_deref() == Some(key.as_str()) {
            if Some(idx) == current_index && !seen.contains(&key) {
                seen.insert(key);
                deduped.push(track);
            } else if current_index.is_none() && seen.insert(key) {
                deduped.push(track);
            }
            continue;
        }

        if seen.insert(key) {
            deduped.push(track);
        }
    }

    // If the current track had duplicates before it, the loop above keeps the current
    // occurrence and drops earlier duplicates. If there was no current track, it keeps
    // the first occurrence of every track.
    if deduped.is_empty() && !app.queue.is_empty() {
        deduped.push(app.queue[0].clone());
    }

    app.queue = deduped;
    app.reset_shuffle_play_order();
    let removed = original_len.saturating_sub(app.queue.len());

    if let Some(key) = current_key {
        app.current_queue_index = app.queue.iter().position(|track| queue_track_key(track) == key);
    }
    if app.current_queue_index.is_none() && !app.queue.is_empty() {
        app.current_queue_index = Some(0);
    }
    if let Some(index) = app.current_queue_index {
        app.ensure_queue_index_visible(index);
    }
    app.enter_queue_context();
    queue_playback_plan_changed(app);

    if removed == 0 {
        app.push_message("Queue dedupe complete: no duplicate tracks found.".to_string());
    } else {
        app.push_message(format!(
            "Queue dedupe complete: removed {} duplicate track(s), {} track(s) remain. Current selection was preserved.",
            removed,
            app.queue.len()
        ));
    }
    refresh_queue_view_if_active(app, refresh_queue_view);
}

fn handle_trim_queue_before_current_command(app: &mut AppState) {
    let refresh_queue_view = matches!(app.selection_context, Some(SelectionContext::Queue));

    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return;
    }
    let Some(current_index) = app.current_queue_index else {
        app.push_message("No current queue item is selected.".to_string());
        return;
    };
    if current_index == 0 {
        app.push_message("There are no played/prior queue items before the current selection.".to_string());
        refresh_queue_view_if_active(app, refresh_queue_view);
        return;
    }

    let removed = current_index;
    app.queue.drain(0..current_index);
    app.reset_shuffle_play_order();
    app.current_queue_index = Some(0);
    app.queue_page_start = 0;
    app.enter_queue_context();
    queue_playback_plan_changed(app);
    app.push_message(format!(
        "Removed {} prior queue item(s). Current track is now queue item 1.",
        removed
    ));
    refresh_queue_view_if_active(app, refresh_queue_view);
}

fn handle_trim_queue_after_current_command(app: &mut AppState) {
    let refresh_queue_view = matches!(app.selection_context, Some(SelectionContext::Queue));

    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return;
    }
    let Some(current_index) = app.current_queue_index else {
        app.push_message("No current queue item is selected.".to_string());
        return;
    };
    if current_index + 1 >= app.queue.len() {
        app.push_message("There are no upcoming queue items after the current selection.".to_string());
        refresh_queue_view_if_active(app, refresh_queue_view);
        return;
    }

    let removed = app.queue.len().saturating_sub(current_index + 1);
    app.queue.truncate(current_index + 1);
    app.reset_shuffle_play_order();
    app.ensure_queue_index_visible(current_index);
    app.enter_queue_context();
    queue_playback_plan_changed(app);
    app.push_message(format!(
        "Removed {} upcoming queue item(s). Current track was preserved.",
        removed
    ));
    refresh_queue_view_if_active(app, refresh_queue_view);
}

fn handle_remove_command(app: &mut AppState, args: &str) -> Result<()> {
    let refresh_queue_view = matches!(app.selection_context, Some(SelectionContext::Queue));

    if args.is_empty() {
        app.push_message("Usage: remove <queue-number...>".to_string());
        return Ok(());
    }
    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return Ok(());
    }

    let mut indices = parse_queue_number_list(args, "remove")?;
    if indices.is_empty() {
        app.push_message("Usage: remove <queue-number...>".to_string());
        return Ok(());
    }

    let queue_len = app.queue.len();
    for &idx in &indices {
        if idx >= queue_len {
            app.push_message(format!("No queue item at number {}.", idx + 1));
            return Ok(());
        }
    }

    let old_current_index = app.current_queue_index;
    let current_key = old_current_index
        .and_then(|idx| app.queue.get(idx))
        .map(queue_track_key);
    let current_removed = old_current_index
        .map(|idx| indices.contains(&idx))
        .unwrap_or(false);
    let playback_state = app
        .playback
        .as_ref()
        .map(|engine| engine.snapshot().state)
        .unwrap_or(PlaybackState::Stopped);

    indices.sort_unstable_by(|a, b| b.cmp(a));
    let mut removed_labels = Vec::new();
    for idx in indices {
        let removed = app.queue.remove(idx);
        removed_labels.push(app.track_label(&removed));
    }
    removed_labels.reverse();

    app.reset_shuffle_play_order();
    if app.queue.is_empty() {
        app.queue_playlist_name = None;
        app.current_queue_index = None;
        app.queue_page_start = 0;
        app.selection_context = None;
        app.paused_selection_pending = false;
        app.user_pause_requested = false;
        if let Some(engine) = &app.playback {
            engine.stop();
        }
        app.push_message(format!("Removed {} queue item(s). Queue is now empty.", removed_labels.len()));
        return Ok(());
    }

    if current_removed {
        let replacement_index = old_current_index
            .unwrap_or(0)
            .min(app.queue.len().saturating_sub(1));
        app.current_queue_index = Some(replacement_index);
    } else if let Some(key) = current_key {
        app.current_queue_index = app.queue.iter().position(|track| queue_track_key(track) == key);
    }
    if app.current_queue_index.is_none() {
        app.current_queue_index = Some(0);
    }
    if let Some(index) = app.current_queue_index {
        app.ensure_queue_index_visible(index);
    }
    app.enter_queue_context();
    queue_playback_plan_changed(app);

    let selected_label = app
        .current_queue_index
        .and_then(|idx| app.queue.get(idx))
        .map(|track| app.track_label(track))
        .unwrap_or_else(|| "none".to_string());
    app.push_message(format!(
        "Removed {} queue item(s). Current: {}",
        removed_labels.len(),
        selected_label
    ));

    let refreshed_by_playback_restart = if current_removed {
        match playback_state {
            PlaybackState::Playing | PlaybackState::Buffering => {
                let index = app.current_queue_index.unwrap_or(0);
                play_queue_index(app, index)?;
                true
            }
            PlaybackState::Paused => {
                app.paused_selection_pending = true;
                app.push_message("Removed the paused queue item. Playback remains paused; press Space or use 'play' to start the selected replacement.".to_string());
                false
            }
            PlaybackState::Stopped | PlaybackState::Error => {
                app.paused_selection_pending = false;
                false
            }
        }
    } else {
        false
    };

    if !refreshed_by_playback_restart {
        refresh_queue_view_if_active(app, refresh_queue_view);
    }

    Ok(())
}

fn handle_move_command(app: &mut AppState, args: &str) -> Result<()> {
    let refresh_queue_view = matches!(app.selection_context, Some(SelectionContext::Queue));
    let parts: Vec<&str> = args.split_whitespace().collect();
    if parts.len() != 2 {
        app.push_message("Usage: move <from> <to>".to_string());
        return Ok(());
    }
    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return Ok(());
    }

    let from = parts[0]
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Move source must be a queue number."))?;
    let to = parts[1]
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("Move destination must be a queue number."))?;
    if from == 0 || to == 0 {
        return Err(anyhow::anyhow!("Queue numbers are 1-based."));
    }
    let from_idx = from - 1;
    let to_idx = to - 1;
    if from_idx >= app.queue.len() {
        app.push_message(format!("No queue item at number {}.", from));
        return Ok(());
    }
    if to_idx >= app.queue.len() {
        app.push_message(format!("No queue position at number {}.", to));
        return Ok(());
    }
    if from_idx == to_idx {
        app.push_message(format!("Queue item {} is already at position {}.", from, to));
        return Ok(());
    }

    app.shuffle_original_order = None;
    app.reset_shuffle_play_order();
    let current_key = app
        .current_queue_index
        .and_then(|idx| app.queue.get(idx))
        .map(queue_track_key);

    let moved = app.queue.remove(from_idx);
    let moved_label = app.track_label(&moved);
    app.queue.insert(to_idx, moved);

    if let Some(key) = current_key {
        app.current_queue_index = app.queue.iter().position(|track| queue_track_key(track) == key);
    } else if !app.queue.is_empty() {
        app.current_queue_index = Some(0);
    }
    if let Some(index) = app.current_queue_index {
        app.ensure_queue_index_visible(index);
    }
    app.enter_queue_context();
    queue_playback_plan_changed(app);

    app.push_message(format!("Moved queue item {} to {}: {}", from, to, moved_label));
    refresh_queue_view_if_active(app, refresh_queue_view);
    Ok(())
}

fn queue_track_key(track: &QueueTrack) -> String {
    format!("{}|{}", track.server_alias, track.id)
}

fn queue_order_keys(tracks: &[QueueTrack]) -> Vec<String> {
    tracks.iter().map(queue_track_key).collect()
}

fn split_server_prefix<'a>(app: &'a AppState, command: &'a str) -> Option<(&'a str, &'a str)> {
    let trimmed = command.trim();
    let mut parts = trimmed.splitn(2, ' ');
    let first = parts.next()?.trim();
    let remainder = parts.next()?.trim();
    if remainder.is_empty() {
        return None;
    }
    if is_reserved_server_alias(first) {
        return None;
    }
    if app.config.find_server(first).is_some() {
        Some((first, remainder))
    } else {
        None
    }
}


fn query_has_wildcard(query: &str) -> bool {
    query.chars().any(|ch| matches!(ch, '*' | '?'))
}

fn wildcard_literal_segments(pattern: &str) -> Vec<String> {
    let mut segments = Vec::new();
    let mut current = String::new();

    for ch in pattern.chars() {
        if matches!(ch, '*' | '?') {
            push_unique_segment(&mut segments, current.trim());
            current.clear();
        } else {
            current.push(ch);
        }
    }
    push_unique_segment(&mut segments, current.trim());

    let existing = segments.clone();
    for segment in existing {
        for word in segment.split_whitespace() {
            push_unique_segment(&mut segments, word.trim());
        }
    }

    segments.sort_by_key(|segment| std::cmp::Reverse(segment.chars().count()));
    segments
}

fn push_unique_segment(segments: &mut Vec<String>, segment: &str) {
    let segment = segment.trim();
    if segment.is_empty() || !segment.chars().any(|ch| ch.is_alphanumeric()) {
        return;
    }
    if !segments.iter().any(|existing| existing.eq_ignore_ascii_case(segment)) {
        segments.push(segment.to_string());
    }
}

fn wildcard_search_seeds(pattern: &str) -> Vec<String> {
    let mut seeds = Vec::new();

    for segment in wildcard_literal_segments(pattern) {
        if segment.chars().count() >= 2 {
            push_unique_segment(&mut seeds, &segment);
        }
    }

    let stripped = pattern
        .chars()
        .map(|ch| if matches!(ch, '*' | '?') { ' ' } else { ch })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if stripped.chars().count() >= 2 {
        push_unique_segment(&mut seeds, &stripped);
    }

    seeds.sort_by_key(|seed| std::cmp::Reverse(seed.chars().count()));
    seeds.truncate(5);
    seeds
}

fn contains_wildcard_pattern(pattern: &str) -> String {
    let mut normalized = pattern.trim().to_string();
    if normalized.is_empty() {
        return normalized;
    }
    if !normalized.starts_with('*') {
        normalized.insert(0, '*');
    }
    if !normalized.ends_with('*') {
        normalized.push('*');
    }
    normalized
}

fn wildcard_match_contains(query: &str, text: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    if query_has_wildcard(query) {
        wildcard_matches(&contains_wildcard_pattern(query), text)
    } else {
        text.to_lowercase().contains(&query.to_lowercase())
    }
}

fn wildcard_matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.to_lowercase().chars().collect();
    let text: Vec<char> = text.to_lowercase().chars().collect();
    let mut dp = vec![vec![false; text.len() + 1]; pattern.len() + 1];
    dp[0][0] = true;

    for i in 1..=pattern.len() {
        if pattern[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }

    for i in 1..=pattern.len() {
        for j in 1..=text.len() {
            dp[i][j] = match pattern[i - 1] {
                '*' => dp[i - 1][j] || dp[i][j - 1],
                '?' => dp[i - 1][j - 1],
                ch => ch == text[j - 1] && dp[i - 1][j - 1],
            };
        }
    }

    dp[pattern.len()][text.len()]
}

fn query_matches_title(query: &str, title: &str) -> bool {
    let query = query.trim();
    if query.is_empty() {
        return true;
    }
    if query_has_wildcard(query) {
        wildcard_matches(&contains_wildcard_pattern(query), title)
    } else {
        title.to_lowercase().contains(&query.to_lowercase())
    }
}

fn result_identity(item: &SearchResultItem) -> String {
    format!(
        "{:?}|{}|{}|{}",
        item.kind,
        item.server_alias,
        item.target_id.as_deref().unwrap_or(""),
        item.title.to_lowercase()
    )
}

fn filter_cached_items(mut items: Vec<SearchResultItem>, query: &str, count: usize) -> Vec<SearchResultItem> {
    let query = query.trim();
    if !query.is_empty() {
        items.retain(|item| query_matches_title(query, &item.title));
    }
    if items.len() > count {
        items.truncate(count);
    }
    items
}

fn append_unique_results(target: &mut Vec<SearchResultItem>, seen: &mut HashSet<String>, items: impl IntoIterator<Item = SearchResultItem>, count: usize) {
    for item in items {
        if seen.insert(result_identity(&item)) {
            target.push(item);
            if target.len() >= count {
                break;
            }
        }
    }
}

async fn album_library_fallback_search(client: &SubsonicClient, query: &str, count: usize) -> Result<Vec<SearchResultItem>> {
    let scan_limit = count.saturating_mul(100).max(5000).min(20000);
    let albums = client.get_all_albums_for_search(scan_limit).await?;
    Ok(filter_cached_items(albums, query, count))
}

async fn cached_genres_for(app: &mut AppState, client: &SubsonicClient) -> Result<Vec<SearchResultItem>> {
    let alias = client.alias().to_string();
    if let Some(items) = app.genre_cache.get(&alias) {
        return Ok(items.clone());
    }
    let items = client.search_genre("", 5000).await?;
    app.genre_cache.insert(alias, items.clone());
    Ok(items)
}

async fn cached_playlists_for(app: &mut AppState, client: &SubsonicClient) -> Result<Vec<SearchResultItem>> {
    let alias = client.alias().to_string();
    if let Some(items) = app.playlist_cache.get(&alias) {
        return Ok(items.clone());
    }
    let items = client.search_playlists("", 5000).await?;
    app.playlist_cache.insert(alias, items.clone());
    Ok(items)
}

async fn cached_artists_for(app: &mut AppState, client: &SubsonicClient) -> Result<Vec<SearchResultItem>> {
    let alias = client.alias().to_string();
    if let Some(items) = app.artist_cache.get(&alias) {
        return Ok(items.clone());
    }
    let items = client.get_all_artists(5000).await?;
    app.artist_cache.insert(alias, items.clone());
    Ok(items)
}

async fn search3_kind_with_optional_wildcard(
    app: &mut AppState,
    client: &SubsonicClient,
    query: &str,
    kind: ResultKind,
    count: usize,
    label: &str,
) -> Result<Vec<SearchResultItem>> {
    let query = query.trim();
    if query_has_wildcard(query) {
        let seeds = wildcard_search_seeds(query);
        if seeds.is_empty() {
            app.push_message(format!(
                "Wildcard {} search '{}' is too broad. Include at least one non-wildcard word or letter.",
                label,
                query
            ));
            return Ok(Vec::new());
        }

        let mut seen = HashSet::new();
        let mut matches = Vec::new();
        for seed in seeds {
            let items = client.search3(&seed, count.saturating_mul(6).max(200)).await?;
            append_unique_results(
                &mut matches,
                &mut seen,
                items.into_iter().filter(|item| item.kind == kind && query_matches_title(query, &item.title)),
                count,
            );
            if matches.len() >= count {
                return Ok(matches);
            }
        }

        if kind == ResultKind::Album && matches.len() < count {
            let fallback = album_library_fallback_search(client, query, count).await?;
            append_unique_results(&mut matches, &mut seen, fallback, count);
        }
        return Ok(matches);
    }

    let mut items = client.search3(query, count).await?;
    items.retain(|item| item.kind == kind);
    if kind == ResultKind::Album && items.is_empty() && query.chars().filter(|ch| ch.is_alphanumeric()).count() >= 2 {
        return album_library_fallback_search(client, query, count).await;
    }
    Ok(items)
}

fn show_cache_status(app: &mut AppState) {
    if app.genre_cache.is_empty() && app.playlist_cache.is_empty() && app.artist_cache.is_empty() {
        app.push_message("Wildcard caches are empty. Use genres/playlists/artists searches to populate them.".to_string());
        return;
    }

    let mut lines = Vec::new();
    for server in &app.config.servers {
        let alias = &server.alias;
        let genres = app.genre_cache.get(alias).map(|items| items.len()).unwrap_or(0);
        let playlists = app.playlist_cache.get(alias).map(|items| items.len()).unwrap_or(0);
        let artists = app.artist_cache.get(alias).map(|items| items.len()).unwrap_or(0);
        if genres > 0 || playlists > 0 || artists > 0 {
            lines.push(format!(
                "Cache [{}]: {} genres, {} playlists, {} artists",
                alias,
                genres,
                playlists,
                artists
            ));
        }
    }

    if lines.is_empty() {
        app.push_message("Wildcard caches are empty. Use genres/playlists/artists searches to populate them.".to_string());
    } else {
        for line in lines {
            app.push_message(line);
        }
    }
}

fn clear_wildcard_caches(app: &mut AppState) {
    app.genre_cache.clear();
    app.playlist_cache.clear();
    app.artist_cache.clear();
    app.push_message("Cleared genre, playlist, and artist wildcard caches.".to_string());
}


fn strip_any_command_prefix<'a>(command: &'a str, prefixes: &[&str]) -> Option<&'a str> {
    for prefix in prefixes {
        if command.eq_ignore_ascii_case(prefix) {
            return Some("");
        }
        let spaced = format!("{} ", prefix);
        if let Some(rest) = strip_command_prefix(command, &spaced) {
            return Some(rest.trim());
        }
    }
    None
}

async fn handle_server_playlist_command(app: &mut AppState, explicit_server: Option<&str>, command: &str) -> Result<bool> {
    let trimmed = command.trim();

    if let Some(rest) = strip_any_command_prefix(trimmed, &["playlist-save", "playlist-create", "pl-save", "pl-create", "pl save", "pl create", "ps"]) {
        handle_server_playlist_save(app, explicit_server, rest).await?;
        return Ok(true);
    }

    if let Some(rest) = strip_any_command_prefix(trimmed, &["playlist-update", "pl-update", "pl update", "pu"]) {
        handle_server_playlist_update(app, explicit_server, rest).await?;
        return Ok(true);
    }

    if let Some(rest) = strip_any_command_prefix(trimmed, &["playlist-add", "playlist-add-to", "pl-add", "pl-add-to", "pl add", "pl add-to", "pa"]) {
        handle_server_playlist_add(app, explicit_server, rest).await?;
        return Ok(true);
    }

    if let Some(rest) = strip_any_command_prefix(trimmed, &["playlist-delete", "pl-delete", "pl delete", "pd"]) {
        handle_server_playlist_delete(app, explicit_server, rest).await?;
        return Ok(true);
    }

    if let Some(rest) = strip_any_command_prefix(trimmed, &["playlist-rename", "pl-rename", "pl rename", "pr"]) {
        handle_server_playlist_rename(app, explicit_server, rest).await?;
        return Ok(true);
    }

    Ok(false)
}

async fn handle_server_playlist_save(app: &mut AppState, explicit_server: Option<&str>, name: &str) -> Result<()> {
    let name = name.trim();
    if name.is_empty() {
        app.push_message("Usage: playlist-save <name>, pl save <name>, or ps <name>. Saves the current queue as a server-side Subsonic playlist.".to_string());
        return Ok(());
    }

    let client = playlist_write_client_for_current_queue(app, explicit_server)?;
    let alias = client.alias().to_string();
    let song_ids = queue_song_ids_for_server(app, &alias)?;
    client.create_playlist(name, &song_ids).await?;
    app.playlist_cache.remove(&alias);
    app.queue_playlist_name = Some(name.to_string());
    app.push_message(format!(
        "Created server playlist '{}' on {} with {} track(s).",
        name,
        alias,
        song_ids.len()
    ));
    Ok(())
}

async fn handle_server_playlist_update(app: &mut AppState, explicit_server: Option<&str>, target: &str) -> Result<()> {
    let target = target.trim();
    if target.is_empty() {
        app.push_message("Usage: playlist-update <name|number|id>, pl update <target>, or pu <target>. Replaces that server playlist with the current queue.".to_string());
        return Ok(());
    }

    let Some((client, playlist)) = resolve_server_playlist_target(app, explicit_server, target).await? else {
        return Ok(());
    };
    let alias = client.alias().to_string();
    let song_ids = queue_song_ids_for_server(app, &alias)?;
    client.replace_playlist(&playlist.id, &song_ids).await?;
    app.playlist_cache.remove(&alias);
    app.push_message(format!(
        "Updated server playlist '{}' [{}] on {} with {} current-queue track(s).",
        playlist.name,
        playlist.id,
        alias,
        song_ids.len()
    ));
    Ok(())
}

async fn handle_server_playlist_add(app: &mut AppState, explicit_server: Option<&str>, args: &str) -> Result<()> {
    let args = args.trim();
    if args.is_empty() {
        app.push_message("Usage: playlist-add <name|number|id> [items], pl add <target> [items], or pa <target> [items]. Without items, appends the current queue.".to_string());
        return Ok(());
    }

    let (target_text, item_args) = split_playlist_add_target_and_items(args);
    if target_text.trim().is_empty() {
        app.push_message("playlist-add needs a playlist name, number, or id before item numbers.".to_string());
        return Ok(());
    }

    let Some((client, playlist)) = resolve_server_playlist_target(app, explicit_server, &target_text).await? else {
        return Ok(());
    };
    let alias = client.alias().to_string();
    let tracks = if let Some(items) = item_args {
        collect_playlist_add_tracks(app, &items).await?
    } else {
        app.queue.clone()
    };
    let song_ids = song_ids_from_tracks_for_server(&tracks, &alias)?;
    if song_ids.is_empty() {
        app.push_message("No tracks to add to the server playlist.".to_string());
        return Ok(());
    }

    client.append_to_playlist(&playlist.id, &song_ids).await?;
    app.playlist_cache.remove(&alias);
    app.push_message(format!(
        "Added {} track(s) to server playlist '{}' on {}.",
        song_ids.len(),
        playlist.name,
        alias
    ));
    Ok(())
}

async fn handle_server_playlist_delete(app: &mut AppState, explicit_server: Option<&str>, target: &str) -> Result<()> {
    let target = target.trim();
    if target.is_empty() {
        app.push_message("Usage: playlist-delete <name|number|id>, pl delete <target>, or pd <target>. Deletes a server-side Subsonic playlist.".to_string());
        return Ok(());
    }

    let Some((client, playlist)) = resolve_server_playlist_target(app, explicit_server, target).await? else {
        return Ok(());
    };
    let alias = client.alias().to_string();
    client.delete_playlist(&playlist.id).await?;
    app.playlist_cache.remove(&alias);
    app.push_message(format!(
        "Deleted server playlist '{}' [{}] on {}.",
        playlist.name,
        playlist.id,
        alias
    ));
    Ok(())
}

async fn handle_server_playlist_rename(app: &mut AppState, explicit_server: Option<&str>, args: &str) -> Result<()> {
    let args = args.trim();
    let Some((target_text, new_name)) = split_playlist_rename_args(args) else {
        app.push_message("Usage: playlist-rename <target> to <new-name>, pl rename <target> to <new-name>, or pr <target> to <new-name>.".to_string());
        return Ok(());
    };

    let Some((client, playlist)) = resolve_server_playlist_target(app, explicit_server, &target_text).await? else {
        return Ok(());
    };
    let alias = client.alias().to_string();
    client.rename_playlist(&playlist.id, &new_name).await?;
    app.playlist_cache.remove(&alias);
    app.push_message(format!(
        "Renamed server playlist '{}' to '{}' on {}.",
        playlist.name,
        new_name,
        alias
    ));
    Ok(())
}

fn playlist_write_client_for_current_queue(app: &AppState, explicit_server: Option<&str>) -> Result<SubsonicClient> {
    if app.queue.is_empty() {
        return Err(anyhow::anyhow!("Queue is empty. Add tracks before saving a server playlist."));
    }

    if let Some(target) = explicit_server {
        return app.target_client(Some(target));
    }

    let aliases = queue_server_aliases(&app.queue);
    if aliases.len() == 1 {
        return app.target_client(Some(&aliases[0]));
    }

    Err(anyhow::anyhow!(
        "Queue contains tracks from multiple servers ({}). Use a server prefix such as '<alias> playlist-save <name>', or save a single-server queue.",
        aliases.join(", ")
    ))
}

fn queue_server_aliases(tracks: &[QueueTrack]) -> Vec<String> {
    let mut aliases = Vec::new();
    for track in tracks {
        if !aliases.iter().any(|alias: &String| alias.eq_ignore_ascii_case(&track.server_alias)) {
            aliases.push(track.server_alias.clone());
        }
    }
    aliases
}

fn queue_song_ids_for_server(app: &AppState, server_alias: &str) -> Result<Vec<String>> {
    if app.queue.is_empty() {
        return Err(anyhow::anyhow!("Queue is empty."));
    }
    song_ids_from_tracks_for_server(&app.queue, server_alias)
}

fn song_ids_from_tracks_for_server(tracks: &[QueueTrack], server_alias: &str) -> Result<Vec<String>> {
    let mut ids = Vec::new();
    let mut wrong_servers = Vec::new();
    for track in tracks {
        if track.server_alias.eq_ignore_ascii_case(server_alias) {
            ids.push(track.id.clone());
        } else if !wrong_servers.iter().any(|alias: &String| alias.eq_ignore_ascii_case(&track.server_alias)) {
            wrong_servers.push(track.server_alias.clone());
        }
    }

    if !wrong_servers.is_empty() {
        return Err(anyhow::anyhow!(
            "Server playlist target is '{}', but selected tracks also include server(s): {}.",
            server_alias,
            wrong_servers.join(", ")
        ));
    }

    Ok(ids)
}

async fn resolve_server_playlist_target(
    app: &mut AppState,
    explicit_server: Option<&str>,
    target: &str,
) -> Result<Option<(SubsonicClient, ServerPlaylistTarget)>> {
    let target = target.trim();
    if target.is_empty() {
        return Ok(None);
    }

    if let Ok(number) = target.parse::<usize>() {
        if number == 0 {
            return Err(anyhow::anyhow!("Playlist numbers are 1-based."));
        }

        if let Some(item) = app.results.as_ref().and_then(|state| state.items.get(number - 1)).cloned() {
            if item.kind == ResultKind::Playlist {
                if let Some(explicit) = explicit_server {
                    if !item.server_alias.eq_ignore_ascii_case(explicit) {
                        app.push_message(format!(
                            "Playlist result {} belongs to server '{}', not '{}'.",
                            number,
                            item.server_alias,
                            explicit
                        ));
                        return Ok(None);
                    }
                }
                let client = app.target_client(Some(&item.server_alias))?;
                return Ok(Some((client, ServerPlaylistTarget {
                    id: item.target_id.unwrap_or_default(),
                    name: item.title,
                    server_alias: item.server_alias,
                })));
            }
        }

        let client = app.target_client(explicit_server)?;
        let playlists = cached_playlists_for(app, &client).await?;
        if let Some(item) = playlists.get(number - 1).cloned() {
            return Ok(Some((client, ServerPlaylistTarget {
                id: item.target_id.unwrap_or_default(),
                name: item.title,
                server_alias: item.server_alias,
            })));
        }
        app.push_message(format!("No server playlist number {} on {}.", number, client.alias()));
        return Ok(None);
    }

    let client = app.target_client(explicit_server)?;
    let playlists = cached_playlists_for(app, &client).await?;
    let exact: Vec<SearchResultItem> = playlists
        .iter()
        .filter(|item| item.title.eq_ignore_ascii_case(target))
        .cloned()
        .collect();

    match exact.len() {
        1 => {
            let item = exact.into_iter().next().unwrap();
            Ok(Some((client, ServerPlaylistTarget {
                id: item.target_id.unwrap_or_default(),
                name: item.title,
                server_alias: item.server_alias,
            })))
        }
        n if n > 1 => {
            app.push_message(format!(
                "Playlist name '{}' is ambiguous on {} ({} matches). Use 'playlists {}' and then the playlist number.",
                target,
                client.alias(),
                n,
                target
            ));
            Ok(None)
        }
        _ => {
            let alias = client.alias().to_string();
            if target.split_whitespace().count() > 1 {
                app.push_message(format!(
                    "No playlist named '{}' found on {}. Use 'playlists {}' to search, or use a raw single-token playlist id.",
                    target,
                    alias,
                    target
                ));
                return Ok(None);
            }
            app.push_message(format!(
                "No playlist named '{}' found on {}. Treating it as a raw playlist id.",
                target,
                alias
            ));
            Ok(Some((client, ServerPlaylistTarget {
                id: target.to_string(),
                name: target.to_string(),
                server_alias: alias,
            })))
        }
    }
}

fn split_playlist_add_target_and_items(args: &str) -> (String, Option<String>) {
    let tokens: Vec<&str> = args.split_whitespace().collect();
    if tokens.len() <= 1 {
        return (args.trim().to_string(), None);
    }

    let mut suffix_start = tokens.len();
    while suffix_start > 1 && token_looks_like_item_selector(tokens[suffix_start - 1]) {
        suffix_start -= 1;
    }

    if suffix_start == tokens.len() {
        (args.trim().to_string(), None)
    } else {
        (
            tokens[..suffix_start].join(" "),
            Some(tokens[suffix_start..].join(" ")),
        )
    }
}

fn token_looks_like_item_selector(token: &str) -> bool {
    let trimmed = token.trim();
    trimmed == "*" || looks_like_add_args(trimmed)
}

fn split_playlist_rename_args(args: &str) -> Option<(String, String)> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some(pos) = find_case_insensitive_separator(trimmed, " to ") {
        let target = trimmed[..pos].trim();
        let new_name = trimmed[pos + 4..].trim();
        if !target.is_empty() && !new_name.is_empty() {
            return Some((target.to_string(), new_name.to_string()));
        }
        return None;
    }

    let mut parts = trimmed.splitn(2, ' ');
    let target = parts.next()?.trim();
    let new_name = parts.next()?.trim();
    if target.is_empty() || new_name.is_empty() {
        return None;
    }

    if looks_like_positive_number(target) || target.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
        return Some((target.to_string(), new_name.to_string()));
    }

    None
}

fn find_case_insensitive_separator(haystack: &str, needle: &str) -> Option<usize> {
    haystack.to_ascii_lowercase().find(&needle.to_ascii_lowercase())
}

async fn collect_playlist_add_tracks(app: &mut AppState, args: &str) -> Result<Vec<QueueTrack>> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Ok(app.queue.clone());
    }

    match app.selection_context {
        Some(SelectionContext::Queue) => {
            if trimmed == "*" {
                return Ok(app.queue.clone());
            }
            let indices = parse_queue_number_list(trimmed, "playlist-add")?;
            let mut tracks = Vec::new();
            for idx in indices {
                let Some(track) = app.queue.get(idx).cloned() else {
                    app.push_message(format!("No queue item at number {}.", idx + 1));
                    continue;
                };
                tracks.push(track);
            }
            Ok(tracks)
        }
        Some(SelectionContext::Recent) => {
            if trimmed == "*" {
                return Ok(app.recent_tracks.clone());
            }
            let indices = parse_queue_number_list(trimmed, "playlist-add")?;
            let mut tracks = Vec::new();
            for idx in indices {
                let Some(track) = app.recent_tracks.get(idx).cloned() else {
                    app.push_message(format!("No playback-history track at number {}.", idx + 1));
                    continue;
                };
                tracks.push(track);
            }
            Ok(tracks)
        }
        Some(SelectionContext::Results) | None => {
            if app.results.is_none() {
                app.push_message("No current results. Use queue view or run a search before playlist-add with item numbers.".to_string());
                return Ok(Vec::new());
            }
            let indices: Vec<usize> = if trimmed == "*" {
                app.results
                    .as_ref()
                    .map(|state| (0..state.items.len()).collect())
                    .unwrap_or_default()
            } else {
                parse_result_number_list(app, trimmed, "playlist-add")?
            };
            collect_tracks_from_result_indices(app, &indices).await
        }
        Some(SelectionContext::SavedQueues) => {
            app.push_message("playlist-add cannot read tracks directly from saved-queue list numbers yet. Load or append the saved queue first.".to_string());
            Ok(Vec::new())
        }
        Some(SelectionContext::Messages) => {
            app.push_message("playlist-add cannot use message-log numbers. Use view results, view queue, or history first.".to_string());
            Ok(Vec::new())
        }
        Some(SelectionContext::Help) => {
            app.push_message("playlist-add cannot use command-help lines. Use view results, view queue, or history first.".to_string());
            Ok(Vec::new())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RandomKind {
    Albums,
    Tracks,
}

#[derive(Clone, Debug)]
struct RandomRequest {
    kind: RandomKind,
    count: usize,
    genre_filter: Option<String>,
}

fn parse_recent_album_count(command: &str) -> Result<Option<usize>> {
    let parts: Vec<&str> = command.split_whitespace().collect();
    let lower: Vec<String> = parts.iter().map(|part| part.to_ascii_lowercase()).collect();
    let words: Vec<&str> = lower.iter().map(|word| word.as_str()).collect();

    match words.as_slice() {
        ["rec"] | ["recent"] | ["recent", "album"] | ["recent", "albums"] => {
            Ok(Some(DEFAULT_RECENT_ALBUM_COUNT))
        }
        ["rec", count] | ["recent", count] => {
            Ok(Some(parse_positive_count(count, "recent")?))
        }
        ["recent", "album", count] | ["recent", "albums", count] => {
            Ok(Some(parse_positive_count(count, "recent albums")?))
        }
        _ => Ok(None),
    }
}

fn parse_random_request(command: &str) -> Result<Option<RandomRequest>> {
    let parts: Vec<&str> = command.split_whitespace().collect();
    if parts.is_empty() {
        return Ok(None);
    }

    let first = parts[0].to_ascii_lowercase();
    if first != "rnd" && first != "random" {
        return Ok(None);
    }

    let mut kind = RandomKind::Albums;
    let mut count: Option<usize> = None;
    let mut genre_filter: Option<String> = None;
    let mut i = 1usize;

    while i < parts.len() {
        let word = parts[i].to_ascii_lowercase();
        match word.as_str() {
            "t" | "track" | "tracks" | "song" | "songs" => {
                kind = RandomKind::Tracks;
                i += 1;
            }
            "album" | "albums" => {
                kind = RandomKind::Albums;
                i += 1;
            }
            "g" | "genre" | "genres" => {
                let filter = parts[i + 1..].join(" ");
                if filter.trim().is_empty() {
                    return Err(anyhow::anyhow!(
                        "Random genre filter needs a genre name or wildcard. Try 'rnd 5 g ?rock' or 'rnd t 50 g folk'."
                    ));
                }
                genre_filter = Some(filter.trim().to_string());
                break;
            }
            _ if count.is_none() => {
                let label = if kind == RandomKind::Tracks { "random tracks" } else { "random albums" };
                count = Some(parse_positive_count(parts[i], label)?);
                i += 1;
            }
            _ => {
                return Err(anyhow::anyhow!(
                    "Unrecognised random argument '{}'. Try 'rnd [n]', 'rnd t [n]', 'rnd [n] g <genre>', or 'rnd t [n] g <genre>'.",
                    parts[i]
                ));
            }
        }
    }

    let count = count.unwrap_or(match kind {
        RandomKind::Albums => DEFAULT_RANDOM_ALBUM_COUNT,
        RandomKind::Tracks => DEFAULT_RANDOM_TRACK_COUNT,
    });

    Ok(Some(RandomRequest { kind, count, genre_filter }))
}

fn parse_positive_count(value: &str, command_name: &str) -> Result<usize> {
    let count = value.parse::<usize>().map_err(|_| {
        anyhow::anyhow!("Invalid count '{}' for {}. Use a positive whole number.", value, command_name)
    })?;
    if count == 0 {
        return Err(anyhow::anyhow!("Count for {} must be at least 1.", command_name));
    }
    Ok(count)
}


fn queue_track_to_result_item(track: &QueueTrack) -> SearchResultItem {
    let mut parts = vec![track.artist.clone(), track.album.clone()];
    if let Some(genre) = track.genre.as_deref().filter(|value| !value.trim().is_empty()) {
        parts.push(format!("genre '{}'", genre));
    }

    SearchResultItem {
        kind: ResultKind::Track,
        title: track.title.clone(),
        subtitle: parts.join(" • "),
        server_alias: track.server_alias.clone(),
        playable: true,
        target_id: Some(track.id.clone()),
    }
}

fn random_genre_title(kind: RandomKind, count: usize, genre_filter: &str, genre_names: &[String], alias: &str) -> String {
    let kind_label = match kind {
        RandomKind::Albums => "albums",
        RandomKind::Tracks => "tracks",
    };
    let genre_label = if genre_names.len() == 1 {
        format!("genre '{}'", genre_names[0])
    } else {
        format!("{} matching genres for '{}'", genre_names.len(), genre_filter)
    };
    format!("Random {} by {} on {} ({})", kind_label, genre_label, alias, count)
}

async fn resolve_random_genres(app: &mut AppState, client: &SubsonicClient, genre_filter: &str) -> Result<Vec<String>> {
    let genre_filter = genre_filter.trim();
    let mut genre_names: Vec<String> = cached_genres_for(app, client)
        .await?
        .into_iter()
        .filter(|item| query_matches_title(genre_filter, &item.title))
        .map(|item| item.title)
        .filter(|title| !title.trim().is_empty())
        .collect();

    genre_names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    genre_names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));

    if genre_names.is_empty() {
        app.push_message(format!(
            "No genres matched '{}'. Try 'genres {}' to inspect matches, or use wildcards like '*rock' / '?rock'.",
            genre_filter,
            genre_filter
        ));
    }

    Ok(genre_names)
}

async fn random_albums_for_genres(
    app: &mut AppState,
    client: &SubsonicClient,
    count: usize,
    genre_filter: &str,
) -> Result<(Vec<SearchResultItem>, Vec<String>)> {
    let genre_names = resolve_random_genres(app, client, genre_filter).await?;
    let mut seen = HashSet::new();
    let mut albums = Vec::new();

    for genre in &genre_names {
        for album in client.get_genre_albums(genre).await? {
            if seen.insert(result_identity(&album)) {
                albums.push(album);
            }
        }
    }

    albums.shuffle(&mut rand::thread_rng());
    if albums.len() > count {
        albums.truncate(count);
    }

    Ok((albums, genre_names))
}

async fn random_tracks_for_genres(
    app: &mut AppState,
    client: &SubsonicClient,
    count: usize,
    genre_filter: &str,
) -> Result<(Vec<SearchResultItem>, Vec<String>)> {
    let genre_names = resolve_random_genres(app, client, genre_filter).await?;
    let mut seen = HashSet::new();
    let mut tracks = Vec::new();

    for genre in &genre_names {
        for track in client.get_genre_queue_tracks(genre).await? {
            let key = queue_track_key(&track);
            if seen.insert(key) {
                tracks.push(track);
            }
        }
    }

    tracks.shuffle(&mut rand::thread_rng());
    if tracks.len() > count {
        tracks.truncate(count);
    }

    Ok((tracks.iter().map(queue_track_to_result_item).collect(), genre_names))
}

async fn collect_search_items_for_client(app: &mut AppState, client: &SubsonicClient, trimmed: &str) -> Result<Option<Vec<SearchResultItem>>> {
    if trimmed.eq_ignore_ascii_case("starred")
        || trimmed.eq_ignore_ascii_case("favorites")
        || trimmed.eq_ignore_ascii_case("favourites")
        || trimmed.eq_ignore_ascii_case("favs")
    {
        return Ok(Some(client.get_starred2().await?));
    }

    if let Some(count) = parse_recent_album_count(trimmed)? {
        return Ok(Some(client.get_recent_albums(count).await?));
    }

    if let Some(random_request) = parse_random_request(trimmed)? {
        let items = match (random_request.kind, random_request.genre_filter.as_deref()) {
            (RandomKind::Albums, Some(genre_filter)) => random_albums_for_genres(app, client, random_request.count, genre_filter).await?.0,
            (RandomKind::Tracks, Some(genre_filter)) => random_tracks_for_genres(app, client, random_request.count, genre_filter).await?.0,
            (RandomKind::Albums, None) => client.get_random_albums(random_request.count).await?,
            (RandomKind::Tracks, None) => client.get_random_tracks(random_request.count).await?,
        };
        return Ok(Some(items));
    }

    if trimmed.eq_ignore_ascii_case("genres") {
        return Ok(Some(cached_genres_for(app, client).await?));
    }

    if let Some(rest) = trimmed.strip_prefix("g ")
        .or_else(|| trimmed.strip_prefix("genre "))
        .or_else(|| trimmed.strip_prefix("genres "))
    {
        return Ok(Some(filter_cached_items(cached_genres_for(app, client).await?, rest.trim(), 500)));
    }

    if trimmed.eq_ignore_ascii_case("artists") {
        return Ok(Some(cached_artists_for(app, client).await?));
    }

    if let Some(rest) = strip_command_prefix(trimmed, "artist ")
        .or_else(|| strip_command_prefix(trimmed, "artists "))
        .or_else(|| strip_command_prefix(trimmed, "ar "))
        .or_else(|| strip_command_prefix(trimmed, "art "))
    {
        return Ok(Some(filter_cached_items(cached_artists_for(app, client).await?, rest.trim(), 500)));
    }

    if let Some(rest) = strip_command_prefix(trimmed, "album ")
        .or_else(|| strip_command_prefix(trimmed, "albums "))
        .or_else(|| strip_command_prefix(trimmed, "al "))
    {
        return Ok(Some(search3_kind_with_optional_wildcard(app, client, rest.trim(), ResultKind::Album, 75, "album").await?));
    }

    if let Some(rest) = strip_command_prefix(trimmed, "track ")
        .or_else(|| strip_command_prefix(trimmed, "tracks "))
        .or_else(|| strip_command_prefix(trimmed, "tr "))
        .or_else(|| strip_command_prefix(trimmed, "song "))
        .or_else(|| strip_command_prefix(trimmed, "songs "))
    {
        return Ok(Some(search3_kind_with_optional_wildcard(app, client, rest.trim(), ResultKind::Track, 100, "track").await?));
    }

    if trimmed.eq_ignore_ascii_case("playlists") || trimmed.eq_ignore_ascii_case("pls") || trimmed.eq_ignore_ascii_case("pl") {
        return Ok(Some(cached_playlists_for(app, client).await?));
    }

    if let Some(rest) = trimmed.strip_prefix("playlist ")
        .or_else(|| trimmed.strip_prefix("playlists "))
        .or_else(|| trimmed.strip_prefix("pl "))
    {
        return Ok(Some(filter_cached_items(cached_playlists_for(app, client).await?, rest.trim(), 500)));
    }

    if let Some(rest) = strip_command_prefix(trimmed, "search ")
        .or_else(|| strip_command_prefix(trimmed, "s "))
    {
        let query = rest.trim();
        let items = if query_has_wildcard(query) {
            let seeds = wildcard_search_seeds(query);
            if seeds.is_empty() {
                Vec::new()
            } else {
                let mut seen = HashSet::new();
                let mut matches = Vec::new();
                for seed in seeds {
                    let items = client.search3(&seed, 200).await?;
                    append_unique_results(
                        &mut matches,
                        &mut seen,
                        items.into_iter().filter(|item| query_matches_title(query, &item.title)),
                        200,
                    );
                }
                if matches.len() < 200 {
                    let fallback = album_library_fallback_search(client, query, 200).await?;
                    append_unique_results(&mut matches, &mut seen, fallback, 200);
                }
                matches
            }
        } else {
            client.search3(query, 50).await?
        };
        return Ok(Some(items));
    }

    Ok(None)
}

fn all_search_title(command: &str, server_count: usize) -> String {
    let trimmed = command.trim();
    if let Some(rest) = strip_command_prefix(trimmed, "search ")
        .or_else(|| strip_command_prefix(trimmed, "s "))
    {
        return format!("All-server search results for '{}' ({} servers)", rest.trim(), server_count);
    }
    if let Some(rest) = strip_command_prefix(trimmed, "album ")
        .or_else(|| strip_command_prefix(trimmed, "albums "))
        .or_else(|| strip_command_prefix(trimmed, "al "))
    {
        return format!("All-server album results for '{}' ({} servers)", rest.trim(), server_count);
    }
    if let Some(rest) = strip_command_prefix(trimmed, "track ")
        .or_else(|| strip_command_prefix(trimmed, "tracks "))
        .or_else(|| strip_command_prefix(trimmed, "tr "))
        .or_else(|| strip_command_prefix(trimmed, "song "))
        .or_else(|| strip_command_prefix(trimmed, "songs "))
    {
        return format!("All-server track results for '{}' ({} servers)", rest.trim(), server_count);
    }
    if let Some(rest) = strip_command_prefix(trimmed, "artist ")
        .or_else(|| strip_command_prefix(trimmed, "artists "))
        .or_else(|| strip_command_prefix(trimmed, "ar "))
        .or_else(|| strip_command_prefix(trimmed, "art "))
    {
        return format!("All-server artist results for '{}' ({} servers)", rest.trim(), server_count);
    }
    if let Some(rest) = trimmed.strip_prefix("g ")
        .or_else(|| trimmed.strip_prefix("genre "))
        .or_else(|| trimmed.strip_prefix("genres "))
    {
        return format!("All-server genre results for '{}' ({} servers)", rest.trim(), server_count);
    }
    if let Some(rest) = trimmed.strip_prefix("playlist ")
        .or_else(|| trimmed.strip_prefix("playlists "))
        .or_else(|| trimmed.strip_prefix("pl "))
    {
        return format!("All-server playlist results for '{}' ({} servers)", rest.trim(), server_count);
    }
    if trimmed.eq_ignore_ascii_case("playlists") || trimmed.eq_ignore_ascii_case("pls") || trimmed.eq_ignore_ascii_case("pl") {
        return format!("All-server playlists ({} servers)", server_count);
    }
    if trimmed.eq_ignore_ascii_case("genres") {
        return format!("All-server genres ({} servers)", server_count);
    }
    if trimmed.eq_ignore_ascii_case("artists") {
        return format!("All-server artists ({} servers)", server_count);
    }
    if parse_recent_album_count(trimmed).ok().flatten().is_some() {
        return format!("All-server recent albums ({} servers)", server_count);
    }
    if parse_random_request(trimmed).ok().flatten().is_some() {
        return format!("All-server random results ({} servers)", server_count);
    }
    if trimmed.eq_ignore_ascii_case("starred")
        || trimmed.eq_ignore_ascii_case("favorites")
        || trimmed.eq_ignore_ascii_case("favourites")
        || trimmed.eq_ignore_ascii_case("favs")
    {
        return format!("All-server starred items ({} servers)", server_count);
    }
    format!("All-server results for '{}' ({} servers)", trimmed, server_count)
}

fn flag_body(token: &str) -> Option<String> {
    let trimmed = token.trim();
    if !trimmed.starts_with('-') {
        return None;
    }
    let body = trimmed.trim_start_matches('-');
    if body.is_empty() {
        return None;
    }
    Some(body.to_ascii_lowercase())
}

fn flag_is(token: &str, names: &[&str]) -> bool {
    flag_body(token)
        .as_deref()
        .map(|body| names.iter().any(|name| body == *name))
        .unwrap_or(false)
}

fn strip_trailing_search_queue_action(command: &str) -> (String, Option<SearchQueueAction>) {
    let mut tokens: Vec<&str> = command.split_whitespace().collect();
    if tokens.is_empty() {
        return (String::new(), None);
    }

    let mut play = false;
    let mut replace = false;
    let mut consumed = 0usize;

    while let Some(last) = tokens.last().copied() {
        let Some(flag) = flag_body(last) else {
            break;
        };
        match flag.as_str() {
            "play" | "autoplay" | "p" => {
                play = true;
                tokens.pop();
                consumed += 1;
            }
            "replace" | "overwrite" | "clear" | "r" => {
                replace = true;
                tokens.pop();
                consumed += 1;
            }
            "append" | "add" | "a" => {
                replace = false;
                tokens.pop();
                consumed += 1;
            }
            "rp" | "pr" => {
                replace = true;
                play = true;
                tokens.pop();
                consumed += 1;
            }
            _ => break,
        }
    }

    if consumed == 0 || !play {
        return (command.trim().to_string(), None);
    }

    let action = if replace {
        SearchQueueAction::ReplacePlay
    } else {
        SearchQueueAction::AppendPlay
    };
    (tokens.join(" "), Some(action))
}

async fn queue_all_current_results_and_play(app: &mut AppState, action: SearchQueueAction) -> Result<()> {
    let indices: Vec<usize> = app
        .results
        .as_ref()
        .map(|results| (0..results.items.len()).collect())
        .unwrap_or_default();
    if indices.is_empty() {
        app.push_message("No search results to queue.".to_string());
        return Ok(());
    }

    let tracks = collect_tracks_from_result_indices(app, &indices).await?;
    if tracks.is_empty() {
        app.push_message("Search results did not contain any playable tracks to queue.".to_string());
        return Ok(());
    }

    match action {
        SearchQueueAction::ReplacePlay => {
            let count = tracks.len();
            let first_key = tracks.first().map(queue_track_key);
            app.set_queue(tracks);
            if let Some(key) = first_key {
                app.current_queue_index = app.queue.iter().position(|track| queue_track_key(track) == key).or(Some(0));
            } else {
                app.current_queue_index = Some(0);
            }
            app.ensure_queue_index_visible(app.current_queue_index.unwrap_or(0));
            app.push_message(format!("Replaced queue with {} track(s) from the search results and started playback.", count));
            autoplay_after_queue_change(app, true, true)?;
        }
        SearchQueueAction::AppendPlay => {
            let count = tracks.len();
            let first_key = tracks.first().map(queue_track_key);
            app.append_queue(tracks);
            if let Some(key) = first_key {
                app.current_queue_index = app.queue.iter().position(|track| queue_track_key(track) == key);
            }
            if app.current_queue_index.is_none() && !app.queue.is_empty() {
                app.current_queue_index = Some(0);
            }
            app.ensure_queue_index_visible(app.current_queue_index.unwrap_or(0));
            app.push_message(format!("Appended {} search-result track(s) and started playback at the first appended track.", count));
            autoplay_after_queue_change(app, false, true)?;
        }
    }
    Ok(())
}

fn server_timeout_seconds(server: &ServerConfig) -> u64 {
    server.search_timeout_seconds.clamp(5, 600)
}

fn search_timeout_for_spec(config: &AppConfig, all_servers: bool, explicit_server: Option<&str>) -> u64 {
    if all_servers {
        return config
            .servers
            .iter()
            .map(server_timeout_seconds)
            .max()
            .unwrap_or(config.search_timeout_seconds.clamp(5, 600));
    }
    explicit_server
        .and_then(|alias| config.find_server(alias))
        .or_else(|| config.primary_server())
        .map(server_timeout_seconds)
        .unwrap_or(config.search_timeout_seconds.clamp(5, 600))
}

fn is_likely_search_command(command: &str) -> bool {
    let first = command.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
    matches!(
        first.as_str(),
        "starred" | "favorites" | "favourites" | "favs" |
        "rec" | "recent" | "rnd" | "random" |
        "genres" | "genre" | "g" |
        "artists" | "artist" | "ar" | "art" |
        "albums" | "album" | "al" |
        "tracks" | "track" | "tr" | "song" | "songs" |
        "playlists" | "playlist" | "pls" | "pl" |
        "search" | "s"
    )
}

fn handle_kill_command(app: &mut AppState) {
    let Some(pending) = app.pending_search.take() else {
        app.push_message("No cancellable request is currently running. kill/k currently applies to background search and browse requests.".to_string());
        return;
    };

    let command = pending.command.clone();
    let elapsed = pending.started_at.elapsed().as_secs_f32();
    pending.handle.abort();
    app.force_queue_view_next = false;
    app.push_message(format!(
        "Cancelled request after {:.1}s: {}. You can issue a new command now.",
        elapsed,
        command
    ));
}

fn maybe_start_background_search(app: &mut AppState, command: &str) -> bool {
    if app.pending_search.is_some() {
        app.push_message("A search/browse request is already running. Use kill/k to cancel it before starting another remote request.".to_string());
        return true;
    }

    let mut all_servers = false;
    let mut explicit_server: Option<String> = None;
    let mut search_command = command.trim().to_string();

    if let Some(rest) = strip_command_prefix(search_command.as_str(), "all ")
        .or_else(|| strip_command_prefix(search_command.as_str(), "@all "))
    {
        all_servers = true;
        search_command = rest.trim().to_string();
    } else if let Some((alias, remainder)) = split_server_prefix(app, search_command.as_str()) {
        explicit_server = Some(alias.to_string());
        search_command = remainder.trim().to_string();
    }

    if !is_likely_search_command(search_command.as_str()) {
        return false;
    }

    let config = app.config.clone();
    let display_command = if all_servers {
        format!("all {}", search_command)
    } else if let Some(alias) = explicit_server.as_deref() {
        format!("{} {}", alias, search_command)
    } else {
        search_command.clone()
    };
    let timeout_seconds = search_timeout_for_spec(&config, all_servers, explicit_server.as_deref());
    let per_server = all_servers;
    let force_queue_view = app.force_queue_view_next;
    let (tx, rx) = mpsc::channel();

    app.push_message(format!(
        "Search started: {} | elapsed 0/{}s{}.",
        display_command,
        timeout_seconds,
        if per_server { "/server" } else { "" }
    ));

    let handle = tokio::spawn(async move {
        let result = run_search_task(config, all_servers, explicit_server, search_command).await;
        let _ = tx.send(result);
    });

    app.pending_search = Some(PendingSearch {
        command: display_command,
        started_at: Instant::now(),
        timeout_seconds,
        per_server,
        force_queue_view,
        receiver: rx,
        handle,
    });
    true
}

async fn handle_pending_search(app: &mut AppState) -> Result<()> {
    let received = match app.pending_search.as_ref() {
        Some(pending) => match pending.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(mpsc::TryRecvError::Empty) => None,
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("Search task ended unexpectedly.".to_string())),
        },
        None => None,
    };

    let Some(result) = received else {
        return Ok(());
    };
    let pending = app.pending_search.take();
    let elapsed = pending
        .as_ref()
        .map(|pending| pending.started_at.elapsed().as_secs_f32())
        .unwrap_or(0.0);

    match result {
        Ok(success) => {
            let state = if success.multi_server {
                ResultsState::new_multi_server(success.title, success.items)
            } else {
                ResultsState::new(success.server_alias, success.title, success.items)
            };
            app.set_results(state);
            if !success.errors.is_empty() {
                app.push_message(format!("Some servers failed during all-server search: {}", success.errors.join("; ")));
            }
            app.push_message(format!("Search finished in {:.1}s.", elapsed));
            if let Some(pending) = pending {
                app.force_queue_view_next = pending.force_queue_view;
            }
            if let Some(action) = success.queue_action {
                queue_all_current_results_and_play(app, action).await?;
            }
        }
        Err(error) => {
            app.push_message(format!("Search failed after {:.1}s: {}", elapsed, error));
        }
    }
    app.force_queue_view_next = false;
    Ok(())
}

async fn run_search_task(
    config: AppConfig,
    all_servers: bool,
    explicit_server: Option<String>,
    command: String,
) -> SearchTaskResult {
    if all_servers {
        return run_all_search_task(config, command).await.map_err(|error| error.to_string());
    }
    run_single_search_task(config, explicit_server, command).await.map_err(|error| error.to_string())
}

async fn run_all_search_task(config: AppConfig, command: String) -> Result<SearchTaskSuccess> {
    let (search_command, queue_action) = strip_trailing_search_queue_action(command.as_str());
    let trimmed = search_command.trim();
    if trimmed.is_empty() {
        anyhow::bail!("Usage: all <search command>, for example: all s deep, all al hazards, all tr love, all pl favourites");
    }
    if config.servers.is_empty() {
        anyhow::bail!("No servers are configured. Use add-server to configure your first Subsonic server.");
    }

    let servers = config.servers.clone();
    let mut combined = Vec::new();
    let mut seen = HashSet::new();
    let mut errors = Vec::new();
    let mut recognized = false;

    for server in &servers {
        let client = SubsonicClient::new(server.clone());
        match collect_search_items_for_client_task(&client, trimmed).await {
            Ok(Some((items, _title))) => {
                recognized = true;
                append_unique_results(&mut combined, &mut seen, items, usize::MAX);
            }
            Ok(None) => {}
            Err(error) => errors.push(format!("{}: {}", server.alias, error)),
        }
    }

    if !recognized {
        anyhow::bail!("Usage: all <search command>, for example: all s deep, all al hazards, all tr love, all pl favourites");
    }

    if let Some(random_request) = parse_random_request(trimmed)? {
        combined.shuffle(&mut rand::thread_rng());
        if combined.len() > random_request.count {
            combined.truncate(random_request.count);
        }
    }

    Ok(SearchTaskSuccess {
        server_alias: "all".to_string(),
        title: all_search_title(trimmed, servers.len()),
        items: combined,
        multi_server: true,
        errors,
        queue_action,
    })
}

async fn run_single_search_task(config: AppConfig, explicit_server: Option<String>, command: String) -> Result<SearchTaskSuccess> {
    let (search_command, queue_action) = strip_trailing_search_queue_action(command.as_str());
    let trimmed = search_command.trim();
    let server = match explicit_server.as_deref() {
        Some(alias) => config.find_server(alias).cloned().ok_or_else(|| anyhow::anyhow!("No such server: {}", alias))?,
        None => config.primary_server().cloned().ok_or_else(|| anyhow::anyhow!("No primary server configured. Use add-server first."))?,
    };
    let client = SubsonicClient::new(server);
    let Some((items, title)) = collect_search_items_for_client_task(&client, trimmed).await? else {
        anyhow::bail!(
            "Unrecognised search command. Try rec, rnd, genres, artist/ar/art, album/al, track/tr, playlist/pl, or search/s."
        );
    };
    Ok(SearchTaskSuccess {
        server_alias: client.alias().to_string(),
        title,
        items,
        multi_server: false,
        errors: Vec::new(),
        queue_action,
    })
}

async fn resolve_random_genres_task(client: &SubsonicClient, genre_filter: &str) -> Result<Vec<String>> {
    let genre_filter = genre_filter.trim();
    let mut genre_names: Vec<String> = client
        .search_genre("", 5000)
        .await?
        .into_iter()
        .filter(|item| query_matches_title(genre_filter, &item.title))
        .map(|item| item.title)
        .filter(|title| !title.trim().is_empty())
        .collect();
    genre_names.sort_by(|a, b| a.to_lowercase().cmp(&b.to_lowercase()));
    genre_names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    Ok(genre_names)
}

async fn random_albums_for_genres_task(client: &SubsonicClient, count: usize, genre_filter: &str) -> Result<(Vec<SearchResultItem>, Vec<String>)> {
    let genre_names = resolve_random_genres_task(client, genre_filter).await?;
    let mut seen = HashSet::new();
    let mut albums = Vec::new();
    for genre in &genre_names {
        for album in client.get_genre_albums(genre).await? {
            if seen.insert(result_identity(&album)) {
                albums.push(album);
            }
        }
    }
    albums.shuffle(&mut rand::thread_rng());
    if albums.len() > count {
        albums.truncate(count);
    }
    Ok((albums, genre_names))
}

async fn random_tracks_for_genres_task(client: &SubsonicClient, count: usize, genre_filter: &str) -> Result<(Vec<SearchResultItem>, Vec<String>)> {
    let genre_names = resolve_random_genres_task(client, genre_filter).await?;
    let mut seen = HashSet::new();
    let mut tracks = Vec::new();
    for genre in &genre_names {
        for track in client.get_genre_queue_tracks(genre).await? {
            let key = queue_track_key(&track);
            if seen.insert(key) {
                tracks.push(track);
            }
        }
    }
    tracks.shuffle(&mut rand::thread_rng());
    if tracks.len() > count {
        tracks.truncate(count);
    }
    Ok((tracks.iter().map(queue_track_to_result_item).collect(), genre_names))
}

async fn search3_kind_with_optional_wildcard_task(
    client: &SubsonicClient,
    query: &str,
    kind: ResultKind,
    count: usize,
) -> Result<Vec<SearchResultItem>> {
    let query = query.trim();
    if query_has_wildcard(query) {
        let seeds = wildcard_search_seeds(query);
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        let mut seen = HashSet::new();
        let mut matches = Vec::new();
        for seed in seeds {
            let items = client.search3(&seed, count.saturating_mul(6).max(200)).await?;
            append_unique_results(
                &mut matches,
                &mut seen,
                items.into_iter().filter(|item| item.kind == kind && query_matches_title(query, &item.title)),
                count,
            );
            if matches.len() >= count {
                return Ok(matches);
            }
        }
        if kind == ResultKind::Album && matches.len() < count {
            let fallback = album_library_fallback_search(client, query, count).await?;
            append_unique_results(&mut matches, &mut seen, fallback, count);
        }
        return Ok(matches);
    }
    let mut items = client.search3(query, count).await?;
    items.retain(|item| item.kind == kind);
    if kind == ResultKind::Album && items.is_empty() && query.chars().filter(|ch| ch.is_alphanumeric()).count() >= 2 {
        return album_library_fallback_search(client, query, count).await;
    }
    Ok(items)
}

async fn collect_search_items_for_client_task(client: &SubsonicClient, trimmed: &str) -> Result<Option<(Vec<SearchResultItem>, String)>> {
    if trimmed.eq_ignore_ascii_case("starred")
        || trimmed.eq_ignore_ascii_case("favorites")
        || trimmed.eq_ignore_ascii_case("favourites")
        || trimmed.eq_ignore_ascii_case("favs")
    {
        let items = client.get_starred2().await?;
        return Ok(Some((items, format!("Starred items on {}", client.alias()))));
    }

    if let Some(count) = parse_recent_album_count(trimmed)? {
        let items = client.get_recent_albums(count).await?;
        return Ok(Some((items, format!("Recent albums on {} ({})", client.alias(), count))));
    }

    if let Some(random_request) = parse_random_request(trimmed)? {
        let (items, title) = match (random_request.kind, random_request.genre_filter.as_deref()) {
            (RandomKind::Albums, Some(genre_filter)) => {
                let (items, genre_names) = random_albums_for_genres_task(client, random_request.count, genre_filter).await?;
                let title = random_genre_title(RandomKind::Albums, random_request.count, genre_filter, &genre_names, client.alias());
                (items, title)
            }
            (RandomKind::Tracks, Some(genre_filter)) => {
                let (items, genre_names) = random_tracks_for_genres_task(client, random_request.count, genre_filter).await?;
                let title = random_genre_title(RandomKind::Tracks, random_request.count, genre_filter, &genre_names, client.alias());
                (items, title)
            }
            (RandomKind::Albums, None) => {
                let items = client.get_random_albums(random_request.count).await?;
                (items, format!("Random albums on {} ({})", client.alias(), random_request.count))
            }
            (RandomKind::Tracks, None) => {
                let items = client.get_random_tracks(random_request.count).await?;
                (items, format!("Random tracks on {} ({})", client.alias(), random_request.count))
            }
        };
        return Ok(Some((items, title)));
    }

    if trimmed.eq_ignore_ascii_case("genres") {
        let items = client.search_genre("", 5000).await?;
        return Ok(Some((items, format!("Genres on {}", client.alias()))));
    }

    if let Some(rest) = trimmed.strip_prefix("g ")
        .or_else(|| trimmed.strip_prefix("genre "))
        .or_else(|| trimmed.strip_prefix("genres "))
    {
        let query = rest.trim();
        let items = filter_cached_items(client.search_genre("", 5000).await?, query, 500);
        let title = if query_has_wildcard(query) {
            format!("Wildcard genre results for '{}' on {}", query, client.alias())
        } else {
            format!("Genre results for '{}' on {}", query, client.alias())
        };
        return Ok(Some((items, title)));
    }

    if trimmed.eq_ignore_ascii_case("artists") {
        let items = client.get_all_artists(5000).await?;
        return Ok(Some((items, format!("Artists on {}", client.alias()))));
    }

    if let Some(rest) = strip_command_prefix(trimmed, "artist ")
        .or_else(|| strip_command_prefix(trimmed, "artists "))
        .or_else(|| strip_command_prefix(trimmed, "ar "))
        .or_else(|| strip_command_prefix(trimmed, "art "))
    {
        let query = rest.trim();
        let items = filter_cached_items(client.get_all_artists(5000).await?, query, 500);
        let title = if query_has_wildcard(query) {
            format!("Wildcard artist results for '{}' on {}", query, client.alias())
        } else {
            format!("Artist results for '{}' on {}", query, client.alias())
        };
        return Ok(Some((items, title)));
    }

    if let Some(rest) = strip_command_prefix(trimmed, "album ")
        .or_else(|| strip_command_prefix(trimmed, "albums "))
        .or_else(|| strip_command_prefix(trimmed, "al "))
    {
        let query = rest.trim();
        let items = search3_kind_with_optional_wildcard_task(client, query, ResultKind::Album, 75).await?;
        let title = if query_has_wildcard(query) {
            format!("Wildcard album results for '{}' on {}", query, client.alias())
        } else {
            format!("Album results for '{}' on {}", query, client.alias())
        };
        return Ok(Some((items, title)));
    }

    if let Some(rest) = strip_command_prefix(trimmed, "track ")
        .or_else(|| strip_command_prefix(trimmed, "tracks "))
        .or_else(|| strip_command_prefix(trimmed, "tr "))
        .or_else(|| strip_command_prefix(trimmed, "song "))
        .or_else(|| strip_command_prefix(trimmed, "songs "))
    {
        let query = rest.trim();
        let items = search3_kind_with_optional_wildcard_task(client, query, ResultKind::Track, 100).await?;
        let title = if query_has_wildcard(query) {
            format!("Wildcard track results for '{}' on {}", query, client.alias())
        } else {
            format!("Track results for '{}' on {}", query, client.alias())
        };
        return Ok(Some((items, title)));
    }

    if trimmed.eq_ignore_ascii_case("playlists") || trimmed.eq_ignore_ascii_case("pls") || trimmed.eq_ignore_ascii_case("pl") {
        let items = client.search_playlists("", 5000).await?;
        return Ok(Some((items, format!("Playlists on {}", client.alias()))));
    }

    if let Some(rest) = trimmed.strip_prefix("playlist ")
        .or_else(|| trimmed.strip_prefix("playlists "))
        .or_else(|| trimmed.strip_prefix("pl "))
    {
        let query = rest.trim();
        if matches!(query.split_whitespace().next().unwrap_or("").to_ascii_lowercase().as_str(), "save" | "update" | "add" | "add-to" | "delete" | "rename") {
            return Ok(None);
        }
        let items = filter_cached_items(client.search_playlists("", 5000).await?, query, 500);
        let title = if query_has_wildcard(query) {
            format!("Wildcard playlist results for '{}' on {}", query, client.alias())
        } else {
            format!("Playlist results for '{}' on {}", query, client.alias())
        };
        return Ok(Some((items, title)));
    }

    if let Some(rest) = strip_command_prefix(trimmed, "search ")
        .or_else(|| strip_command_prefix(trimmed, "s "))
    {
        let query = rest.trim();
        let items = if query_has_wildcard(query) {
            let seeds = wildcard_search_seeds(query);
            if seeds.is_empty() {
                Vec::new()
            } else {
                let mut seen = HashSet::new();
                let mut matches = Vec::new();
                for seed in seeds {
                    let items = client.search3(&seed, 200).await?;
                    append_unique_results(
                        &mut matches,
                        &mut seen,
                        items.into_iter().filter(|item| query_matches_title(query, &item.title)),
                        200,
                    );
                }
                if matches.len() < 200 {
                    let fallback = album_library_fallback_search(client, query, 200).await?;
                    append_unique_results(&mut matches, &mut seen, fallback, 200);
                }
                matches
            }
        } else {
            client.search3(query, 50).await?
        };
        let title = if query_has_wildcard(query) {
            format!("Wildcard search results for '{}' on {}", query, client.alias())
        } else {
            format!("Search results for '{}' on {}", query, client.alias())
        };
        return Ok(Some((items, title)));
    }

    Ok(None)
}

async fn execute_all_search_command(app: &mut AppState, command: &str) -> Result<()> {
    let (search_command, queue_action) = strip_trailing_search_queue_action(command);
    let trimmed = search_command.trim();
    if trimmed.is_empty() {
        app.push_message("Usage: all <search command>, for example: all s deep, all al hazards, all tr love, all pl favourites".to_string());
        return Ok(());
    }
    if app.config.servers.is_empty() {
        app.push_message("No servers are configured. Use add-server to configure your first Subsonic server.".to_string());
        return Ok(());
    }

    let servers = app.config.servers.clone();
    let mut combined = Vec::new();
    let mut seen = HashSet::new();
    let mut errors = Vec::new();
    let mut recognized = false;

    for server in &servers {
        let client = SubsonicClient::new(server.clone());
        match collect_search_items_for_client(app, &client, trimmed).await {
            Ok(Some(items)) => {
                recognized = true;
                append_unique_results(&mut combined, &mut seen, items, usize::MAX);
            }
            Ok(None) => {}
            Err(error) => errors.push(format!("{}: {}", server.alias, error)),
        }
    }

    if !recognized {
        app.push_message("Usage: all <search command>, for example: all s deep, all al hazards, all tr love, all pl favourites".to_string());
        return Ok(());
    }

    if let Some(random_request) = parse_random_request(trimmed)? {
        combined.shuffle(&mut rand::thread_rng());
        if combined.len() > random_request.count {
            combined.truncate(random_request.count);
        }
    }

    let title = all_search_title(trimmed, servers.len());
    let state = ResultsState::new_multi_server(title, combined);
    app.set_results(state);
    if !errors.is_empty() {
        app.push_message(format!("Some servers failed during all-server search: {}", errors.join("; ")));
    }
    if let Some(action) = queue_action {
        queue_all_current_results_and_play(app, action).await?;
    }
    Ok(())
}

async fn execute_search_command(app: &mut AppState, explicit_server: Option<&str>, command: &str) -> Result<()> {
    let (search_command, queue_action) = strip_trailing_search_queue_action(command);
    let client = app.target_client(explicit_server)?;
    let trimmed = search_command.trim();

    if trimmed.eq_ignore_ascii_case("starred")
        || trimmed.eq_ignore_ascii_case("favorites")
        || trimmed.eq_ignore_ascii_case("favourites")
        || trimmed.eq_ignore_ascii_case("favs")
    {
        let items = client.get_starred2().await?;
        let state = ResultsState::new(
            client.alias().to_string(),
            format!("Starred items on {}", client.alias()),
            items,
        );
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if let Some(count) = parse_recent_album_count(trimmed)? {
        let items = client.get_recent_albums(count).await?;
        let state = ResultsState::new(
            client.alias().to_string(),
            format!("Recent albums on {} ({})", client.alias(), count),
            items,
        );
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if let Some(random_request) = parse_random_request(trimmed)? {
        let (items, title) = match (random_request.kind, random_request.genre_filter.as_deref()) {
            (RandomKind::Albums, Some(genre_filter)) => {
                let (items, genre_names) = random_albums_for_genres(app, &client, random_request.count, genre_filter).await?;
                let title = random_genre_title(RandomKind::Albums, random_request.count, genre_filter, &genre_names, client.alias());
                (items, title)
            }
            (RandomKind::Tracks, Some(genre_filter)) => {
                let (items, genre_names) = random_tracks_for_genres(app, &client, random_request.count, genre_filter).await?;
                let title = random_genre_title(RandomKind::Tracks, random_request.count, genre_filter, &genre_names, client.alias());
                (items, title)
            }
            (RandomKind::Albums, None) => {
                let items = client.get_random_albums(random_request.count).await?;
                (items, format!("Random albums on {} ({})", client.alias(), random_request.count))
            }
            (RandomKind::Tracks, None) => {
                let items = client.get_random_tracks(random_request.count).await?;
                (items, format!("Random tracks on {} ({})", client.alias(), random_request.count))
            }
        };

        let state = ResultsState::new(client.alias().to_string(), title, items);
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if trimmed.eq_ignore_ascii_case("genres") {
        let items = cached_genres_for(app, &client).await?;
        let state = ResultsState::new(
            client.alias().to_string(),
            format!("Genres on {}", client.alias()),
            items,
        );
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if let Some(rest) = trimmed.strip_prefix("g ")
        .or_else(|| trimmed.strip_prefix("genre "))
        .or_else(|| trimmed.strip_prefix("genres "))
    {
        let query = rest.trim();
        let items = filter_cached_items(cached_genres_for(app, &client).await?, query, 500);
        let title = if query_has_wildcard(query) {
            format!("Wildcard genre results for '{}' on {}", query, client.alias())
        } else {
            format!("Genre results for '{}' on {}", query, client.alias())
        };
        let state = ResultsState::new(client.alias().to_string(), title, items);
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if trimmed.eq_ignore_ascii_case("artists") {
        let items = cached_artists_for(app, &client).await?;
        let state = ResultsState::new(
            client.alias().to_string(),
            format!("Artists on {}", client.alias()),
            items,
        );
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(trimmed, "artist ")
        .or_else(|| strip_command_prefix(trimmed, "artists "))
        .or_else(|| strip_command_prefix(trimmed, "ar "))
        .or_else(|| strip_command_prefix(trimmed, "art "))
    {
        let query = rest.trim();
        let items = filter_cached_items(cached_artists_for(app, &client).await?, query, 500);
        let title = if query_has_wildcard(query) {
            format!("Wildcard artist results for '{}' on {}", query, client.alias())
        } else {
            format!("Artist results for '{}' on {}", query, client.alias())
        };
        let state = ResultsState::new(client.alias().to_string(), title, items);
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(trimmed, "album ")
        .or_else(|| strip_command_prefix(trimmed, "albums "))
        .or_else(|| strip_command_prefix(trimmed, "al "))
    {
        let query = rest.trim();
        let items = search3_kind_with_optional_wildcard(app, &client, query, ResultKind::Album, 75, "album").await?;
        let title = if query_has_wildcard(query) {
            format!("Wildcard album results for '{}' on {}", query, client.alias())
        } else {
            format!("Album results for '{}' on {}", query, client.alias())
        };
        let state = ResultsState::new(client.alias().to_string(), title, items);
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(trimmed, "track ")
        .or_else(|| strip_command_prefix(trimmed, "tracks "))
        .or_else(|| strip_command_prefix(trimmed, "tr "))
        .or_else(|| strip_command_prefix(trimmed, "song "))
        .or_else(|| strip_command_prefix(trimmed, "songs "))
    {
        let query = rest.trim();
        let items = search3_kind_with_optional_wildcard(app, &client, query, ResultKind::Track, 100, "track").await?;
        let title = if query_has_wildcard(query) {
            format!("Wildcard track results for '{}' on {}", query, client.alias())
        } else {
            format!("Track results for '{}' on {}", query, client.alias())
        };
        let state = ResultsState::new(client.alias().to_string(), title, items);
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if trimmed.eq_ignore_ascii_case("playlists") || trimmed.eq_ignore_ascii_case("pls") || trimmed.eq_ignore_ascii_case("pl") {
        let items = cached_playlists_for(app, &client).await?;
        let state = ResultsState::new(
            client.alias().to_string(),
            format!("Playlists on {}", client.alias()),
            items,
        );
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if let Some(rest) = trimmed.strip_prefix("playlist ")
        .or_else(|| trimmed.strip_prefix("playlists "))
        .or_else(|| trimmed.strip_prefix("pl "))
    {
        let query = rest.trim();
        let items = filter_cached_items(cached_playlists_for(app, &client).await?, query, 500);
        let title = if query_has_wildcard(query) {
            format!("Wildcard playlist results for '{}' on {}", query, client.alias())
        } else {
            format!("Playlist results for '{}' on {}", query, client.alias())
        };
        let state = ResultsState::new(client.alias().to_string(), title, items);
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    if let Some(rest) = strip_command_prefix(trimmed, "search ")
        .or_else(|| strip_command_prefix(trimmed, "s "))
    {
        let query = rest.trim();
        let items = if query_has_wildcard(query) {
            let seeds = wildcard_search_seeds(query);
            if seeds.is_empty() {
                app.push_message(format!("Wildcard search '{}' is too broad. Include at least one non-wildcard word or letter.", query));
                Vec::new()
            } else {
                let mut seen = HashSet::new();
                let mut matches = Vec::new();
                for seed in seeds {
                    let items = client.search3(&seed, 200).await?;
                    append_unique_results(
                        &mut matches,
                        &mut seen,
                        items.into_iter().filter(|item| query_matches_title(query, &item.title)),
                        200,
                    );
                }
                if matches.len() < 200 {
                    let fallback = album_library_fallback_search(&client, query, 200).await?;
                    append_unique_results(&mut matches, &mut seen, fallback, 200);
                }
                matches
            }
        } else {
            client.search3(query, 50).await?
        };
        let title = if query_has_wildcard(query) {
            format!("Wildcard search results for '{}' on {}", query, client.alias())
        } else {
            format!("Search results for '{}' on {}", query, client.alias())
        };
        let state = ResultsState::new(client.alias().to_string(), title, items);
        app.set_results(state);
        if let Some(action) = queue_action {
            queue_all_current_results_and_play(app, action).await?;
        }
        return Ok(());
    }

    app.push_message(format!(
        "Unrecognised command. Bare commands route to primary [{}]; prefix with a server alias for another server. Try 'help', 'h', or 'help browse'. Input: {}",
        app.config.primary_display_name(),
        trimmed
    ));
    Ok(())
}


async fn handle_star_command(app: &mut AppState, args: &str, starred: bool) -> Result<()> {
    let args = args.trim();
    let verb = if starred { "star" } else { "unstar" };
    if args.is_empty() {
        app.push_message(format!("Usage: {} <number...>|now|*", verb));
        return Ok(());
    }

    if args.eq_ignore_ascii_case("now") {
        let Some(index) = app.current_queue_index else {
            app.push_message("No current queue item to update.".to_string());
            return Ok(());
        };
        let Some(track) = app.queue.get(index).cloned() else {
            app.push_message("No current queue item to update.".to_string());
            return Ok(());
        };
        let client = app.target_client(Some(&track.server_alias))?;
        if starred {
            client.star_track(&track.id).await?;
        } else {
            client.unstar_track(&track.id).await?;
        }
        app.push_message(format!("{}red current track: {}", if starred { "Star" } else { "Unstar" }, app.track_label(&track)));
        return Ok(());
    }

    match app.selection_context {
        Some(SelectionContext::Queue) => handle_star_queue_items(app, args, starred).await,
        Some(SelectionContext::Recent) => handle_star_recent_items(app, args, starred).await,
        Some(SelectionContext::Results) | None => {
            if app.results.is_some() {
                handle_star_result_items(app, args, starred).await
            } else if !app.queue.is_empty() {
                handle_star_queue_items(app, args, starred).await
            } else {
                app.push_message(format!("No current results or queue items to {}.", verb));
                Ok(())
            }
        }
        Some(SelectionContext::SavedQueues) => {
            app.push_message("Saved queue entries cannot be starred directly. Load a saved queue, then star queue tracks or use 'star now'.".to_string());
            Ok(())
        }
        Some(SelectionContext::Messages) => {
            app.push_message("Message-log entries cannot be starred. Use view results, view queue, or history first.".to_string());
            Ok(())
        }
        Some(SelectionContext::Help) => {
            app.push_message("Command-help lines cannot be starred. Use view results, view queue, or history first.".to_string());
            Ok(())
        }
    }
}

async fn handle_star_recent_items(app: &mut AppState, args: &str, starred: bool) -> Result<()> {
    if app.recent_tracks.is_empty() {
        app.push_message("No playback-history tracks in this session.".to_string());
        return Ok(());
    }

    let indices: Vec<usize> = if args == "*" {
        (0..app.recent_tracks.len()).collect()
    } else {
        parse_queue_number_list(args, if starred { "star" } else { "unstar" })?
    };

    let mut updated = 0usize;
    for idx in indices {
        let Some(track) = app.recent_tracks.get(idx).cloned() else {
            app.push_message(format!("No playback-history track at number {}.", idx + 1));
            continue;
        };
        let client = app.target_client(Some(&track.server_alias))?;
        if starred {
            client.star_track(&track.id).await?;
        } else {
            client.unstar_track(&track.id).await?;
        }
        updated += 1;
    }

    app.push_message(format!(
        "{}red {} playback-history track(s).",
        if starred { "Star" } else { "Unstar" },
        updated
    ));
    Ok(())
}

async fn handle_star_queue_items(app: &mut AppState, args: &str, starred: bool) -> Result<()> {
    if app.queue.is_empty() {
        app.push_message("Queue is empty.".to_string());
        return Ok(());
    }

    let indices: Vec<usize> = if args == "*" {
        (0..app.queue.len()).collect()
    } else {
        parse_queue_number_list(args, if starred { "star" } else { "unstar" })?
    };

    let mut updated = 0usize;
    for idx in indices {
        let Some(track) = app.queue.get(idx).cloned() else {
            app.push_message(format!("No queue item at number {}.", idx + 1));
            continue;
        };
        let client = app.target_client(Some(&track.server_alias))?;
        if starred {
            client.star_track(&track.id).await?;
        } else {
            client.unstar_track(&track.id).await?;
        }
        updated += 1;
    }

    let action = if starred { "Starred" } else { "Unstarred" };
    app.push_message(format!("{} {} queue item(s).", action, updated));
    Ok(())
}

async fn handle_star_result_items(app: &mut AppState, args: &str, starred: bool) -> Result<()> {
    let Some(results) = app.results.as_ref() else {
        app.push_message("No current results.".to_string());
        return Ok(());
    };

    let indices: Vec<usize> = if args == "*" {
        (0..results.items.len()).collect()
    } else {
        parse_result_number_list(app, args, if starred { "star" } else { "unstar" })?
    };

    let mut updated = 0usize;
    let mut skipped = 0usize;
    for idx in indices {
        let Some(item) = app.results.as_ref().and_then(|state| state.items.get(idx)).cloned() else {
            app.push_message(format!("No result at number {}.", idx + 1));
            continue;
        };
        if apply_star_to_result_item(app, &item, starred).await? {
            updated += 1;
        } else {
            skipped += 1;
        }
    }

    let action = if starred { "Starred" } else { "Unstarred" };
    if skipped > 0 {
        app.push_message(format!("{} {} item(s); skipped {} item(s) that Subsonic cannot star directly.", action, updated, skipped));
    } else {
        app.push_message(format!("{} {} item(s).", action, updated));
    }
    Ok(())
}

async fn apply_star_to_result_item(app: &AppState, item: &SearchResultItem, starred: bool) -> Result<bool> {
    let Some(id) = item.target_id.as_deref() else {
        return Ok(false);
    };
    if id.trim().is_empty() {
        return Ok(false);
    }

    let client = app.target_client(Some(&item.server_alias))?;
    match item.kind {
        ResultKind::Track => {
            if starred {
                client.star_track(id).await?;
            } else {
                client.unstar_track(id).await?;
            }
            Ok(true)
        }
        ResultKind::Album => {
            if starred {
                client.star_album(id).await?;
            } else {
                client.unstar_album(id).await?;
            }
            Ok(true)
        }
        ResultKind::Artist => {
            if starred {
                client.star_artist(id).await?;
            } else {
                client.unstar_artist(id).await?;
            }
            Ok(true)
        }
        ResultKind::Playlist | ResultKind::Genre | ResultKind::Section => Ok(false),
    }
}

fn show_download_help(app: &mut AppState) {
    app.push_message("Downloads: download/dl <numbers|*|now> [--replace|-replace|-f]. Works from results, queue, playback history, or saved-queue lists. Albums/playlists try a usable server ZIP first and fall back to individual tracks.".to_string());
    app.push_message("Examples: dl now | dl 1 | dl 1-5 9 | dl * | dl 1 --replace | dl 1 -replace | download-path D:\\Music\\Subsonic.".to_string());
    app.push_message("Track extensions are resolved from Subsonic metadata/headers/audio bytes where possible, so downloads should save as .mp3/.flac/.ogg/.m4a/.wav instead of .bin.".to_string());
    app.push_message(format!("Download folder: {}", current_download_root(app).display()));
    app.push_message(format!("Overwrite existing files: {}. Use download-overwrite on/off, or per-download --replace/-replace/-f / --skip.", if app.config.download_overwrite { "on" } else { "off" }));
}

#[derive(Clone, Copy)]
struct DownloadOptions {
    overwrite: bool,
}

fn parse_download_options(app: &AppState, args: &str) -> (String, DownloadOptions) {
    let mut overwrite = app.config.download_overwrite;
    let mut kept = Vec::new();
    for token in args.split_whitespace() {
        if flag_is(token, &["replace", "overwrite", "force", "r", "f"]) {
            overwrite = true;
        } else if flag_is(token, &["skip", "no-overwrite", "nooverwrite"]) {
            overwrite = false;
        } else {
            kept.push(token.to_string());
        }
    }
    (kept.join(" "), DownloadOptions { overwrite })
}

fn show_download_path_status(app: &mut AppState) {
    app.push_message(format!("Download folder: {}", current_download_root(app).display()));
    match app.config.download_dir.as_deref().filter(|value| !value.trim().is_empty()) {
        Some(value) => app.push_message(format!("Custom download folder saved in config: {}", value)),
        None => app.push_message("Using platform default Downloads/Subsonic TUI Downloads. Use download-path <folder> to set a custom folder, or download-path default to reset.".to_string()),
    }
}

fn handle_download_path_command(app: &mut AppState, value: &str) -> Result<()> {
    let value = value.trim().trim_matches('"');
    if value.is_empty() {
        show_download_path_status(app);
        return Ok(());
    }
    if value.eq_ignore_ascii_case("default") || value.eq_ignore_ascii_case("reset") || value.eq_ignore_ascii_case("clear") {
        app.config.download_dir = None;
        app.persist_config()?;
        app.push_message(format!("Download folder reset to default: {}", current_download_root(app).display()));
        return Ok(());
    }
    let expanded = expand_download_path(value);
    app.config.download_dir = Some(expanded.to_string_lossy().to_string());
    app.persist_config()?;
    app.push_message(format!("Download folder set to {}", current_download_root(app).display()));
    Ok(())
}

fn show_download_overwrite_status(app: &mut AppState) {
    app.push_message(format!(
        "Download overwrite is {}. Use download-overwrite on/off/toggle, or add --replace/-replace/-f to one dl command.",
        if app.config.download_overwrite { "on" } else { "off" }
    ));
}

fn handle_download_overwrite_command(app: &mut AppState, value: &str) -> Result<()> {
    let normalized = value.trim().to_lowercase();
    let next = match normalized.as_str() {
        "on" | "yes" | "true" | "1" => true,
        "off" | "no" | "false" | "0" => false,
        "toggle" => !app.config.download_overwrite,
        _ => {
            app.push_message("Use download-overwrite on, download-overwrite off, or download-overwrite toggle.".to_string());
            return Ok(());
        }
    };
    app.config.download_overwrite = next;
    app.persist_config()?;
    app.push_message(format!("Download overwrite {}.", if next { "enabled" } else { "disabled" }));
    Ok(())
}

async fn handle_download_command(app: &mut AppState, args: &str) -> Result<()> {
    let args = args.trim();
    if args.is_empty() {
        show_download_help(app);
        return Ok(());
    }

    let (args, options) = parse_download_options(app, args);
    let args = args.trim();
    if args.is_empty() {
        show_download_help(app);
        return Ok(());
    }

    if args.eq_ignore_ascii_case("path") || args.eq_ignore_ascii_case("folder") {
        show_download_path_status(app);
        return Ok(());
    }

    if args.eq_ignore_ascii_case("now") {
        let Some(index) = app.current_queue_index else {
            app.push_message("No current queue item to download.".to_string());
            return Ok(());
        };
        let Some(track) = app.queue.get(index).cloned() else {
            app.push_message("No current queue item to download.".to_string());
            return Ok(());
        };
        let written = download_track_to_disk(app, &track, options).await?;
        let written_count = if written { 1 } else { 0 };
        let skipped_count = if written { 0 } else { 1 };
        app.push_message(format!("Download now complete: {} file(s) written, {} existing file(s) skipped.", written_count, skipped_count));
        return Ok(());
    }

    match app.selection_context {
        Some(SelectionContext::Queue) => {
            let tracks = resolve_queue_download_tracks(app, args)?;
            download_track_batch(app, &tracks, "queue selection", options).await?;
        }
        Some(SelectionContext::Recent) => {
            let tracks = resolve_recent_download_tracks(app, args)?;
            download_track_batch(app, &tracks, "playback history", options).await?;
        }
        Some(SelectionContext::Results) | None if app.results.is_some() => {
            let items = resolve_result_download_items(app, args)?;
            download_result_items(app, &items, options).await?;
        }
        Some(SelectionContext::SavedQueues) => {
            download_saved_queue_refs(app, args, options).await?;
        }
        _ if !app.queue.is_empty() => {
            let tracks = resolve_queue_download_tracks(app, args)?;
            download_track_batch(app, &tracks, "queue selection", options).await?;
        }
        _ => {
            app.push_message("Nothing to download. Run a search, use queue, or use dl now while a queue item is selected.".to_string());
        }
    }

    Ok(())
}

fn resolve_recent_download_tracks(app: &AppState, args: &str) -> Result<Vec<QueueTrack>> {
    if app.recent_tracks.is_empty() {
        return Err(anyhow::anyhow!("No playback-history tracks in this session."));
    }
    if args == "*" {
        return Ok(app.recent_tracks.clone());
    }
    let mut tracks = Vec::new();
    for idx in parse_queue_number_list(args, "download")? {
        let Some(track) = app.recent_tracks.get(idx).cloned() else {
            return Err(anyhow::anyhow!("No playback-history track at number {}.", idx + 1));
        };
        tracks.push(track);
    }
    Ok(tracks)
}

fn resolve_queue_download_tracks(app: &AppState, args: &str) -> Result<Vec<QueueTrack>> {
    if app.queue.is_empty() {
        return Err(anyhow::anyhow!("Queue is empty."));
    }
    if args.trim() == "*" {
        return Ok(app.queue.clone());
    }
    let indices = parse_queue_number_list(args, "download")?;
    if indices.is_empty() {
        return Err(anyhow::anyhow!("download needs queue numbers, '*', or 'now'."));
    }
    let mut tracks = Vec::new();
    for idx in indices {
        let track = app
            .queue
            .get(idx)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No queue item at number {}.", idx + 1))?;
        tracks.push(track);
    }
    Ok(tracks)
}

async fn download_saved_queue_refs(app: &mut AppState, args: &str, options: DownloadOptions) -> Result<()> {
    let args = args.trim();
    if args.is_empty() {
        app.push_message("Usage from saved queues: dl <saved-queue-number...> or dl *.".to_string());
        return Ok(());
    }

    let entries = if args == "*" {
        app.saved_queues
            .as_ref()
            .map(|state| state.entries.clone())
            .unwrap_or_default()
    } else {
        let references: Vec<String> = if looks_like_number_selector_args(args, false) {
            parse_number_selector_list(args, "download")?
                .into_iter()
                .map(|idx| (idx + 1).to_string())
                .collect()
        } else {
            args.replace(',', " ")
                .split_whitespace()
                .map(|token| token.to_string())
                .collect()
        };
        let mut selected = Vec::new();
        for reference in references {
            selected.push(resolve_saved_queue_reference(app, &reference, "download")?);
        }
        selected
    };

    if entries.is_empty() {
        app.push_message("No saved queue entries selected. Use 'queues' first.".to_string());
        return Ok(());
    }

    let mut total_written = 0usize;
    let mut total_skipped = 0usize;
    for entry in entries {
        let saved = load_saved_queue_entry(&entry)?;
        let (written, skipped) = download_track_batch_count(app, &saved.tracks, &format!("saved queue '{}'", saved.name), options).await?;
        total_written += written;
        total_skipped += skipped;
    }
    app.push_message(format!("Saved queue download complete: {} file(s) written, {} existing file(s) skipped.", total_written, total_skipped));
    Ok(())
}

fn resolve_result_download_items(app: &AppState, args: &str) -> Result<Vec<SearchResultItem>> {
    let Some(results) = app.results.as_ref() else {
        return Err(anyhow::anyhow!("No current results to download from."));
    };
    if args.trim() == "*" {
        return Ok(results.items.clone());
    }
    let mut items = Vec::new();
    for absolute_idx in parse_result_number_list(app, args, "download")? {
        let item = results
            .items
            .get(absolute_idx)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No result at number {}.", absolute_idx + 1))?;
        items.push(item);
    }
    if items.is_empty() {
        return Err(anyhow::anyhow!("download needs result numbers, '*', or 'now'."));
    }
    Ok(items)
}

async fn download_result_items(app: &mut AppState, items: &[SearchResultItem], options: DownloadOptions) -> Result<()> {
    if items.is_empty() {
        app.push_message("No result items to download.".to_string());
        return Ok(());
    }

    let mut total_written = 0usize;
    let mut skipped = 0usize;
    for item in items {
        match download_result_item(app, item, options).await {
            Ok((written, skipped_item)) => {
                total_written += written;
                skipped += skipped_item;
            }
            Err(error) => app.push_message(format!("Download failed for '{}': {}", item.title, error)),
        }
    }

    app.push_message(format!(
        "Download command complete: {} file(s) written, {} existing file(s) skipped.",
        total_written,
        skipped
    ));
    Ok(())
}

async fn download_result_item(app: &mut AppState, item: &SearchResultItem, options: DownloadOptions) -> Result<(usize, usize)> {
    let client = app.target_client(Some(&item.server_alias))?;
    match item.kind {
        ResultKind::Track => {
            let Some(id) = item.target_id.as_deref() else {
                app.push_message(format!("Track '{}' has no id to download.", item.title));
                return Ok((0, 0));
            };
            let track = client.get_track_by_id(id).await?;
            let written = download_track_to_disk(app, &track, options).await?;
            Ok(if written { (1, 0) } else { (0, 1) })
        }
        ResultKind::Album => {
            let Some(id) = item.target_id.as_deref() else {
                app.push_message(format!("Album '{}' has no id to download.", item.title));
                return Ok((0, 0));
            };
            if let Some(written) = try_download_collection_zip(app, &client, id, "album", &item.title, options).await? {
                return Ok(if written { (1, 0) } else { (0, 1) });
            }
            let tracks = client.get_album_queue_tracks(id).await?;
            download_track_batch_count(app, &tracks, &format!("album '{}'", item.title), options).await
        }
        ResultKind::Playlist => {
            let Some(id) = item.target_id.as_deref() else {
                app.push_message(format!("Playlist '{}' has no id to download.", item.title));
                return Ok((0, 0));
            };
            if let Some(written) = try_download_collection_zip(app, &client, id, "playlist", &item.title, options).await? {
                return Ok(if written { (1, 0) } else { (0, 1) });
            }
            let tracks = client.get_playlist_queue_tracks(id).await?;
            download_track_batch_count(app, &tracks, &format!("playlist '{}'", item.title), options).await
        }
        ResultKind::Artist => {
            let Some(id) = item.target_id.as_deref() else {
                app.push_message(format!("Artist '{}' has no id to download.", item.title));
                return Ok((0, 0));
            };
            let tracks = client.get_artist_queue_tracks(id).await?;
            download_track_batch_count(app, &tracks, &format!("artist '{}'", item.title), options).await
        }
        ResultKind::Genre => {
            let genre = item.target_id.clone().unwrap_or_else(|| item.title.clone());
            let tracks = client.get_genre_queue_tracks(&genre).await?;
            download_track_batch_count(app, &tracks, &format!("genre '{}'", genre), options).await
        }
        ResultKind::Section => {
            app.push_message(format!("'{}' is informational and cannot be downloaded directly.", item.title));
            Ok((0, 0))
        }
    }
}

async fn download_track_batch(app: &mut AppState, tracks: &[QueueTrack], label: &str, options: DownloadOptions) -> Result<()> {
    let (written, skipped) = download_track_batch_count(app, tracks, label, options).await?;
    app.push_message(format!(
        "Downloaded {}: {} file(s) written, {} existing file(s) skipped.",
        label,
        written,
        skipped
    ));
    Ok(())
}

async fn download_track_batch_count(app: &mut AppState, tracks: &[QueueTrack], label: &str, options: DownloadOptions) -> Result<(usize, usize)> {
    if tracks.is_empty() {
        app.push_message(format!("No tracks found for {}.", label));
        return Ok((0, 0));
    }

    app.push_message(format!("Downloading {} track(s) from {}...", tracks.len(), label));
    let mut written = 0usize;
    let mut skipped = 0usize;
    for track in tracks {
        match download_track_to_disk(app, track, options).await {
            Ok(true) => written += 1,
            Ok(false) => skipped += 1,
            Err(error) => app.push_message(format!("Download failed for '{}': {}", app.track_label(track), error)),
        }
    }
    Ok((written, skipped))
}

async fn download_track_to_disk(app: &mut AppState, track: &QueueTrack, options: DownloadOptions) -> Result<bool> {
    let client = app.target_client(Some(&track.server_alias))?;
    let binary = client.download_media(&track.id).await?;
    let mut ext = track_media_extension(track)
        .or_else(|| download_extension(&binary))
        .or_else(|| extension_from_magic_bytes(&binary.bytes));

    if ext.is_none() {
        if let Ok(fresh_track) = client.get_track_by_id(&track.id).await {
            ext = track_media_extension(&fresh_track)
                .or_else(|| download_extension(&binary))
                .or_else(|| extension_from_magic_bytes(&binary.bytes));
        }
    }

    let ext = ext.unwrap_or_else(|| "mp3".to_string());
    let path = track_download_path(&current_download_root(app), track, &ext);
    write_download_binary(app, &path, &binary, options)
}

async fn try_download_collection_zip(
    app: &mut AppState,
    client: &SubsonicClient,
    id: &str,
    kind: &str,
    title: &str,
    options: DownloadOptions,
) -> Result<Option<bool>> {
    app.push_message(format!("Trying server ZIP download for {} '{}'...", kind, title));
    match client.download_media(id).await {
        Ok(binary) if binary.is_zip() && is_plausible_collection_zip(&binary) => {
            let path = collection_zip_path(&current_download_root(app), client.alias(), kind, title);
            let written = write_download_binary(app, &path, &binary, options)?;
            Ok(Some(written))
        }
        Ok(binary) if binary.is_zip() => {
            app.push_message(format!(
                "Server returned a very small ZIP ({} bytes) for {} '{}'; treating server ZIP as unsupported and falling back to individual tracks.",
                binary.bytes.len(),
                kind,
                title
            ));
            Ok(None)
        }
        Ok(binary) => {
            app.push_message(format!(
                "Server did not return a usable ZIP for {} '{}' ({} bytes, content type: {}); falling back to individual tracks.",
                kind,
                title,
                binary.bytes.len(),
                binary.content_type.as_deref().unwrap_or("unknown")
            ));
            Ok(None)
        }
        Err(error) => {
            app.push_message(format!("Server ZIP unavailable for {} '{}': {}. Falling back to individual tracks.", kind, title, concise_error(&error.to_string())));
            Ok(None)
        }
    }
}

const MIN_COLLECTION_ZIP_BYTES: usize = 8 * 1024;

fn is_plausible_collection_zip(binary: &DownloadBinary) -> bool {
    binary.is_zip() && binary.bytes.len() >= MIN_COLLECTION_ZIP_BYTES
}

fn current_download_root(app: &AppState) -> PathBuf {
    app.config
        .download_dir
        .as_deref()
        .filter(|value| !value.trim().is_empty())
        .map(expand_download_path)
        .unwrap_or_else(default_download_root)
}

fn expand_download_path(value: &str) -> PathBuf {
    let trimmed = value.trim().trim_matches('"');
    if trimmed == "~" {
        return dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    }
    if let Some(rest) = trimmed.strip_prefix("~/").or_else(|| trimmed.strip_prefix("~\\")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(trimmed)
}

fn default_download_root() -> PathBuf {
    dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Subsonic TUI Downloads")
}

fn track_download_path(root: &Path, track: &QueueTrack, extension: &str) -> PathBuf {
    let filename = format!("{}.{}", safe_path_component(&track.title), safe_extension(extension));
    root
        .join(safe_path_component(&track.server_alias))
        .join(safe_path_component(&track.artist))
        .join(safe_path_component(&track.album))
        .join(filename)
}

fn collection_zip_path(root: &Path, server_alias: &str, kind: &str, title: &str) -> PathBuf {
    root
        .join(safe_path_component(server_alias))
        .join(format!("{} zips", kind))
        .join(format!("{}.zip", safe_path_component(title)))
}

fn write_download_binary(app: &mut AppState, path: &Path, binary: &DownloadBinary, options: DownloadOptions) -> Result<bool> {
    let existed = path.exists();
    if existed && !options.overwrite {
        app.push_message(format!("Skipped existing download: {}", path.display()));
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, &binary.bytes)?;
    if existed {
        app.push_message(format!("Overwrote: {}", path.display()));
    } else {
        app.push_message(format!("Downloaded: {}", path.display()));
    }
    Ok(true)
}

fn track_media_extension(track: &QueueTrack) -> Option<String> {
    track
        .suffix
        .as_deref()
        .and_then(normalize_media_extension)
        .or_else(|| track.content_type.as_deref().and_then(extension_from_content_type))
}

fn download_extension(binary: &DownloadBinary) -> Option<String> {
    if let Some(disposition) = binary.content_disposition.as_deref() {
        if let Some(ext) = extension_from_content_disposition(disposition) {
            return Some(ext);
        }
    }
    binary
        .content_type
        .as_deref()
        .and_then(extension_from_content_type)
}

fn extension_from_content_type(content_type: &str) -> Option<String> {
    let content_type = content_type.to_ascii_lowercase();
    if content_type.contains("flac") {
        Some("flac".to_string())
    } else if content_type.contains("mpeg") || content_type.contains("mp3") || content_type.contains("audio/mpa") {
        Some("mp3".to_string())
    } else if content_type.contains("ogg") {
        Some("ogg".to_string())
    } else if content_type.contains("opus") {
        Some("opus".to_string())
    } else if content_type.contains("mp4") || content_type.contains("m4a") || content_type.contains("aac") {
        Some("m4a".to_string())
    } else if content_type.contains("wav") || content_type.contains("wave") {
        Some("wav".to_string())
    } else if content_type.contains("aiff") || content_type.contains("aifc") {
        Some("aiff".to_string())
    } else {
        None
    }
}

fn normalize_media_extension(value: &str) -> Option<String> {
    let cleaned = safe_extension(value.trim().trim_start_matches('.'));
    if cleaned == "bin" || cleaned.is_empty() {
        None
    } else {
        Some(match cleaned.as_str() {
            "jpeg" => "jpg".to_string(),
            "mpeg" | "mpga" | "mp2" | "mpa" => "mp3".to_string(),
            "aac" => "m4a".to_string(),
            other => other.to_string(),
        })
    }
}

fn extension_from_magic_bytes(bytes: &[u8]) -> Option<String> {
    if bytes.starts_with(b"fLaC") {
        Some("flac".to_string())
    } else if bytes.starts_with(b"OggS") {
        Some("ogg".to_string())
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12).map(|slice| slice == b"WAVE").unwrap_or(false) {
        Some("wav".to_string())
    } else if bytes.starts_with(b"ID3") || looks_like_mp3_frame(bytes) {
        Some("mp3".to_string())
    } else if bytes.len() >= 12 && bytes.get(4..8).map(|slice| slice == b"ftyp").unwrap_or(false) {
        Some("m4a".to_string())
    } else {
        None
    }
}

fn looks_like_mp3_frame(bytes: &[u8]) -> bool {
    bytes.len() >= 2 && bytes[0] == 0xff && (bytes[1] & 0xe0) == 0xe0
}

fn extension_from_content_disposition(disposition: &str) -> Option<String> {
    let lower = disposition.to_ascii_lowercase();
    let filename_pos = lower.find("filename=")?;
    let raw = disposition[filename_pos + "filename=".len()..]
        .trim()
        .trim_matches('"')
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('"');
    let ext = Path::new(raw).extension()?.to_str()?.to_ascii_lowercase();
    if ext.is_empty() { None } else { Some(ext) }
}

fn safe_extension(extension: &str) -> String {
    let cleaned: String = extension
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect();
    if cleaned.is_empty() { "bin".to_string() } else { cleaned }
}

fn safe_path_component(value: &str) -> String {
    let mut out = String::new();
    let mut last_sep = false;
    for ch in value.trim().chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
            last_sep = false;
        } else if ch.is_whitespace() || matches!(ch, '.' | ',' | '&' | '+' | '(' | ')' | '[' | ']') {
            if !last_sep && !out.is_empty() {
                out.push(' ');
                last_sep = true;
            }
        }
    }
    while out.ends_with(' ') || out.ends_with('.') {
        out.pop();
    }
    if out.is_empty() { "unknown".to_string() } else { out }
}

fn concise_error(value: &str) -> String {
    let mut text = value.replace('\n', " ").replace('\r', " ");
    if text.len() > 180 {
        text.truncate(180);
        text.push_str("...");
    }
    text
}


fn resolve_recent_track(app: &AppState, index: usize) -> Option<QueueTrack> {
    app.recent_tracks.get(index).cloned()
}

fn handle_recent_numeric_selection(app: &mut AppState, index: usize) -> Result<()> {
    let Some(track) = resolve_recent_track(app, index) else {
        app.push_message(format!(
            "No playback-history track at number {}. Playback history list contains {} item(s).",
            index + 1,
            app.recent_tracks.len()
        ));
        return Ok(());
    };

    let label = app.track_label(&track);
    app.set_queue(vec![track]);
    app.push_message(format!("Loaded playback-history track {} into the play queue.", label));
    autoplay_after_queue_change(app, true, true)
}

fn handle_add_recent_command(app: &mut AppState, args: &str) -> Result<()> {
    let args = args.trim();
    if app.recent_tracks.is_empty() {
        app.push_message("No playback-history tracks in this session.".to_string());
        return Ok(());
    }
    if args.is_empty() {
        app.push_message("Usage: add <history-number...> or add * while viewing playback history.".to_string());
        return Ok(());
    }

    let was_empty = app.queue.is_empty();
    let tracks = if args == "*" {
        app.recent_tracks.clone()
    } else {
        let mut tracks = Vec::new();
        for index in parse_queue_number_list(args, "add")? {
            let Some(track) = resolve_recent_track(app, index) else {
                app.push_message(format!(
                    "No playback-history track at number {}. Playback history list contains {} item(s).",
                    index + 1,
                    app.recent_tracks.len()
                ));
                continue;
            };
            tracks.push(track);
        }
        tracks
    };

    let count = tracks.len();
    app.append_queue(tracks);
    if count > 0 {
        app.push_message(format!("Added {} playback-history track(s) to the queue.", count));
    }
    autoplay_after_queue_change(app, was_empty, false)
}

fn looks_like_loose_multi_add_args(value: &str) -> bool {
    if !looks_like_number_selector_args(value, false) {
        return false;
    }
    parse_number_selector_list(value, "selection")
        .map(|indices| indices.len() > 1)
        .unwrap_or(false)
}

async fn handle_loose_multi_add_command(app: &mut AppState, args: &str) -> Result<bool> {
    match app.selection_context {
        Some(SelectionContext::Queue) | Some(SelectionContext::Messages) | Some(SelectionContext::Help) => Ok(false),
        Some(SelectionContext::SavedQueues) => {
            handle_add_saved_queue_command(app, args)?;
            Ok(true)
        }
        Some(SelectionContext::Recent) => {
            handle_add_recent_command(app, args)?;
            Ok(true)
        }
        Some(SelectionContext::Results) | None => {
            if app.results.is_some() {
                handle_add_command(app, args).await?;
                Ok(true)
            } else {
                Ok(false)
            }
        }
    }
}

async fn handle_numeric_selection(app: &mut AppState, index: usize) -> Result<()> {
    match app.selection_context {
        Some(SelectionContext::Queue) => {
            if app.queue.is_empty() {
                app.push_message("Queue is empty.".to_string());
            } else {
                select_queue_index(app, index)?;
            }
        }
        Some(SelectionContext::SavedQueues) => {
            handle_saved_queue_numeric_selection(app, index)?;
        }
        Some(SelectionContext::Recent) => {
            handle_recent_numeric_selection(app, index)?;
        }
        Some(SelectionContext::Messages) => {
            app.push_message("Message numbers are informational; use view results, view queue, view saved, or view history to act on numbered items.".to_string());
        }
        Some(SelectionContext::Help) => {
            app.push_message("Command-help numbers are informational. Use [ / ] or page <n> to navigate help pages.".to_string());
        }
        Some(SelectionContext::Results) | None => {
            if app.results.is_some() {
                handle_result_selection(app, index).await?;
            } else if !app.queue.is_empty() {
                select_queue_index(app, index)?;
            } else {
                app.push_message(format!("No active list item at number {}. Run a search, use 'queue', or use 'queues' first.", index + 1));
            }
        }
    }
    Ok(())
}


async fn handle_play_results_command(app: &mut AppState, args: &str) -> Result<()> {
    if app.results.is_none() {
        app.push_message("No current results to play from. Run a search first, then use play <n>, p <n>, play *, or p *.".to_string());
        return Ok(());
    }

    let indices = if args.trim() == "*" {
        app.results
            .as_ref()
            .map(|results| (0..results.items.len()).collect::<Vec<_>>())
            .unwrap_or_default()
    } else {
        parse_result_number_list(app, args, "play")?
    };
    if indices.is_empty() {
        app.push_message("No selected results to play.".to_string());
        return Ok(());
    }

    let tracks = collect_tracks_from_result_indices(app, &indices).await?;
    if tracks.is_empty() {
        app.push_message("Selected results did not contain any playable tracks.".to_string());
        return Ok(());
    }

    let count = tracks.len();
    let first_key = tracks.first().map(queue_track_key);
    app.append_queue(tracks);
    if let Some(key) = first_key {
        app.current_queue_index = app.queue.iter().position(|track| queue_track_key(track) == key);
    }
    if app.current_queue_index.is_none() && !app.queue.is_empty() {
        app.current_queue_index = Some(0);
    }
    app.ensure_queue_index_visible(app.current_queue_index.unwrap_or(0));
    app.push_message(format!("Appended {} result track(s) and started playback at the first appended track.", count));
    autoplay_after_queue_change(app, false, true)?;
    Ok(())
}

async fn handle_add_command(app: &mut AppState, args: &str) -> Result<()> {
    if matches!(app.selection_context, Some(SelectionContext::SavedQueues)) {
        return handle_add_saved_queue_command(app, args);
    }

    if matches!(app.selection_context, Some(SelectionContext::Recent)) {
        return handle_add_recent_command(app, args);
    }

    if app.results.is_none() {
        app.push_message("No current results to add from. Run a search first, or use 'queues' then a<number> to append a saved queue.");
        return Ok(());
    }

    let was_empty = app.queue.is_empty();

    if args == "*" {
        let indices: Vec<usize> = app
            .results
            .as_ref()
            .map(|results| (0..results.items.len()).collect())
            .unwrap_or_default();
        let tracks = collect_tracks_from_result_indices(app, &indices).await?;
        app.append_queue(tracks);
        autoplay_after_queue_change(app, was_empty, false)?;
        return Ok(());
    }

    let indices = parse_result_number_list(app, args, "add")?;

    let tracks = collect_tracks_from_result_indices(app, &indices).await?;
    app.append_queue(tracks);
    autoplay_after_queue_change(app, was_empty, false)?;
    Ok(())
}

async fn collect_tracks_from_result_indices(app: &AppState, indices: &[usize]) -> Result<Vec<QueueTrack>> {
    let mut out = Vec::new();
    for &idx in indices {
        let item = app
            .results
            .as_ref()
            .and_then(|results| results.items.get(idx))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("No result at absolute index {}", idx + 1))?;
        let client = app.target_client(Some(&item.server_alias))?;
        match item.kind {
            ResultKind::Track => {
                if let Some(id) = item.target_id {
                    out.push(client.get_track_by_id(&id).await?);
                }
            }
            ResultKind::Album => {
                if let Some(id) = item.target_id {
                    out.extend(client.get_album_queue_tracks(&id).await?);
                }
            }
            ResultKind::Artist => {
                if let Some(id) = item.target_id {
                    out.extend(client.get_artist_queue_tracks(&id).await?);
                }
            }
            ResultKind::Genre => {
                let genre = item.target_id.unwrap_or(item.title);
                out.extend(client.get_genre_queue_tracks(&genre).await?);
            }
            ResultKind::Playlist => {
                if let Some(id) = item.target_id {
                    out.extend(client.get_playlist_queue_tracks(&id).await?);
                }
            }
            ResultKind::Section => {}
        }
    }
    Ok(out)
}

async fn handle_result_selection(app: &mut AppState, index: usize) -> Result<()> {
    let selected = match app.resolve_result_selection(index) {
        Some(item) => item,
        None => {
            app.push_message(format!(
                "No result at number {}. Current results contain {} item(s).",
                index + 1,
                app.result_count()
            ));
            return Ok(());
        }
    };

    let client = app.target_client(Some(&selected.server_alias))?;
    match selected.kind {
        ResultKind::Album => play_album_result(app, &client, &selected).await?,
        ResultKind::Playlist => play_playlist_result(app, &client, &selected).await?,
        ResultKind::Track => {
            if let Some(id) = selected.target_id {
                let track = client.get_track_by_id(&id).await?;
                let label = app.track_label(&track);
                app.set_queue(vec![track]);
                app.push_message(format!("Selected track '{}'. Queue now points at it.", label));
                autoplay_after_queue_change(app, true, true)?;
            }
        }
        ResultKind::Artist | ResultKind::Genre | ResultKind::Section => {
            handle_result_explore(app, index).await?;
        }
    }
    Ok(())
}

async fn handle_explore_command(app: &mut AppState, args: &str) -> Result<()> {
    if app.results.is_none() {
        app.push_message("No current results to explore. Use album/playlist search first, then x<number> or explore <number>.".to_string());
        return Ok(());
    }

    let index = parse_single_result_number(app, args, "explore")?;
    handle_result_explore(app, index).await
}

async fn handle_result_explore(app: &mut AppState, index: usize) -> Result<()> {
    let selected = match app.resolve_result_selection(index) {
        Some(item) => item,
        None => {
            app.push_message(format!(
                "No result at number {}. Current results contain {} item(s).",
                index + 1,
                app.result_count()
            ));
            return Ok(());
        }
    };

    let client = app.target_client(Some(&selected.server_alias))?;
    match selected.kind {
        ResultKind::Album => {
            if let Some(id) = selected.target_id {
                let items = client.get_album_tracks(&id).await?;
                let state = ResultsState::new(
                    client.alias().to_string(),
                    format!("Tracks in album '{}' on {}", selected.title, client.alias()),
                    items,
                );
                app.set_results(state);
            } else {
                app.push_message(format!("Album '{}' has no album id to explore.", selected.title));
            }
        }
        ResultKind::Artist => {
            if let Some(id) = selected.target_id {
                let items = client.get_artist_albums(&id).await?;
                let state = ResultsState::new(
                    client.alias().to_string(),
                    format!("Albums by '{}' on {}", selected.title, client.alias()),
                    items,
                );
                app.set_results(state);
            } else {
                app.push_message(format!("Artist '{}' has no artist id to explore.", selected.title));
            }
        }
        ResultKind::Playlist => {
            if let Some(id) = selected.target_id {
                let items = client.get_playlist_tracks(&id).await?;
                let state = ResultsState::new(
                    client.alias().to_string(),
                    format!("Tracks in playlist '{}' on {}", selected.title, client.alias()),
                    items,
                );
                app.set_results(state);
            } else {
                app.push_message(format!("Playlist '{}' has no playlist id to explore.", selected.title));
            }
        }
        ResultKind::Genre => {
            let genre = selected.target_id.unwrap_or_else(|| selected.title.clone());
            let items = client.get_genre_albums(&genre).await?;
            if items.is_empty() {
                app.push_message(format!("No albums found for genre '{}' [{}].", genre, selected.server_alias));
            } else {
                let state = ResultsState::new(
                    client.alias().to_string(),
                    format!("Albums in genre '{}' on {}", genre, client.alias()),
                    items,
                );
                app.set_results(state);
            }
        }
        ResultKind::Track => {
            app.push_message("Track results are already directly playable. Select the track number to replace the queue and play it; use add/a to append it.".to_string());
        }
        ResultKind::Section => {
            app.push_message(format!(
                "Selected '{}' [{}]. This result is informational.",
                selected.title, selected.server_alias
            ));
        }
    }
    Ok(())
}

async fn play_album_result(app: &mut AppState, client: &SubsonicClient, selected: &SearchResultItem) -> Result<()> {
    let Some(id) = selected.target_id.as_deref() else {
        app.push_message(format!("Album '{}' has no album id to play.", selected.title));
        return Ok(());
    };

    let tracks = client.get_album_queue_tracks(id).await?;
    if tracks.is_empty() {
        app.push_message(format!("Album '{}' has no tracks to queue.", selected.title));
        return Ok(());
    }

    let count = tracks.len();
    let result_number = app.resolve_result_index_for_item(selected).map(|idx| idx + 1);
    app.set_queue(tracks);
    if selected.kind == ResultKind::Playlist {
        app.queue_playlist_name = Some(selected.title.clone());
    }
    match result_number {
        Some(number) => app.push_message(format!(
            "Album '{}' sent to play queue as {} track(s). Use 'x {}' or 'explore {}' to view its tracks instead.",
            selected.title,
            count,
            number,
            number
        )),
        None => app.push_message(format!(
            "Album '{}' sent to play queue as {} track(s). Use 'x <n>' or 'explore <n>' to view its tracks instead.",
            selected.title,
            count
        )),
    }
    autoplay_after_queue_change(app, true, true)
}

async fn play_playlist_result(app: &mut AppState, client: &SubsonicClient, selected: &SearchResultItem) -> Result<()> {
    let Some(id) = selected.target_id.as_deref() else {
        app.push_message(format!("Playlist '{}' has no playlist id to play.", selected.title));
        return Ok(());
    };

    let tracks = client.get_playlist_queue_tracks(id).await?;
    if tracks.is_empty() {
        app.push_message(format!("Playlist '{}' has no tracks to queue.", selected.title));
        return Ok(());
    }

    let count = tracks.len();
    let result_number = app.resolve_result_index_for_item(selected).map(|idx| idx + 1);
    app.set_queue(tracks);
    if selected.kind == ResultKind::Playlist {
        app.queue_playlist_name = Some(selected.title.clone());
    }
    match result_number {
        Some(number) => app.push_message(format!(
            "Playlist '{}' sent to play queue as {} track(s). Use 'x {}' or 'explore {}' to view its tracks instead.",
            selected.title,
            count,
            number,
            number
        )),
        None => app.push_message(format!(
            "Playlist '{}' sent to play queue as {} track(s). Use 'x <n>' or 'explore <n>' to view its tracks instead.",
            selected.title,
            count
        )),
    }
    autoplay_after_queue_change(app, true, true)
}

fn parse_single_result_number(app: &AppState, args: &str, command_name: &str) -> Result<usize> {
    let trimmed = args.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("{} needs a result number.", command_name));
    }
    if trimmed.contains(',') || trimmed.split_whitespace().count() != 1 {
        return Err(anyhow::anyhow!("{} takes exactly one result number.", command_name));
    }
    let number = trimmed
        .parse::<usize>()
        .map_err(|_| anyhow::anyhow!("{} needs a positive result number.", command_name))?;
    if number == 0 {
        return Err(anyhow::anyhow!("Result numbers are 1-based."));
    }
    app.resolve_result_index(number - 1)
        .ok_or_else(|| anyhow::anyhow!("No result at number {}. Current results contain {} item(s).", number, app.result_count()))
}

fn parse_result_number_list(app: &AppState, args: &str, command_name: &str) -> Result<Vec<usize>> {
    let mut out = Vec::new();
    for zero_based in parse_number_selector_list(args, command_name)? {
        let number = zero_based + 1;
        let index = app
            .resolve_result_index(zero_based)
            .ok_or_else(|| anyhow::anyhow!("No result at number {}. Current results contain {} item(s).", number, app.result_count()))?;
        if !out.contains(&index) {
            out.push(index);
        }
    }
    Ok(out)
}

fn looks_like_positive_number(value: &str) -> bool {
    let trimmed = value.trim();
    !trimmed.is_empty() && trimmed.chars().all(|ch| ch.is_ascii_digit()) && trimmed.parse::<usize>().map(|n| n > 0).unwrap_or(false)
}

fn looks_like_add_args(value: &str) -> bool {
    looks_like_number_selector_args(value, true)
}

fn show_doctor_report(app: &mut AppState, check_download_write: bool) -> Result<()> {
    app.push_message(format!("Doctor: DISC {} diagnostics", BUILD_LABEL));
    app.push_message(format!("Config: {}", app.store.path().display()));
    if app.store.path().exists() {
        app.push_message("Config file: present.".to_string());
    } else {
        app.push_message("Config file: not created yet; defaults are active until you save a setting/server.".to_string());
    }

    match app.config.primary_server() {
        Some(server) => app.push_message(format!(
            "Primary server: {} [{}] -> {}",
            server.name, server.alias, server.base_url
        )),
        None => app.push_message("Primary server: none configured. Use add-server, then use <alias> if needed.".to_string()),
    }

    if app.config.servers.is_empty() {
        app.push_message("Servers: none configured.".to_string());
    } else {
        let aliases: Vec<String> = app.config.servers.iter().map(|server| server.alias.clone()).collect();
        app.push_message(format!("Servers: {} configured ({})", aliases.len(), aliases.join(", ")));
        let mut seen = HashSet::new();
        let duplicates: Vec<String> = aliases
            .iter()
            .filter_map(|alias| {
                let key = alias.to_lowercase();
                if seen.insert(key) { None } else { Some(alias.clone()) }
            })
            .collect();
        if !duplicates.is_empty() {
            app.push_message(format!("Warning: duplicate server aliases detected: {}", duplicates.join(", ")));
        }
    }

    if let Some(engine) = app.playback.as_ref() {
        let snapshot = engine.snapshot();
        let audio_status = engine.audio_status();
        app.push_message(format!(
            "Playback engine: available | state: {:?} | volume: {} | repeat: {} | gapless setting: {} | media keys: {} | active audio: {} | OS default audio: {}",
            snapshot.state,
            snapshot.volume,
            app.repeat_mode.label(),
            on_off(app.config.gapless_playback),
            on_off(app.config.media_keys_enabled),
            audio_status.active_output.as_deref().unwrap_or("unknown"),
            audio_status.default_output.as_deref().unwrap_or("none reported")
        ));
        if let Some(error) = audio_status.last_reset_error {
            app.push_message(format!("Playback audio reset warning: {}", error));
        }
    } else {
        app.push_message(format!(
            "Playback engine: unavailable ({})",
            app.playback_init_error
                .as_deref()
                .unwrap_or("audio output could not be initialised")
        ));
    }

    app.push_message(format!(
        "Queue: {} track(s), current selection: {}",
        app.queue.len(),
        app.current_queue_index
            .map(|idx| (idx + 1).to_string())
            .unwrap_or_else(|| "none".to_string())
    ));
    app.push_message(format!("Playback history: {} item(s) in this session.", app.recent_tracks.len()));

    match count_toml_files(&app.store.queue_dir()) {
        Ok(count) => app.push_message(format!("Saved queues: {} file(s) in {}", count, app.store.queue_dir().display())),
        Err(error) => app.push_message(format!("Saved queues: could not inspect {} ({})", app.store.queue_dir().display(), error)),
    }

    let session_path = app.store.session_path();
    app.push_message(format!(
        "Last session: {} ({})",
        if session_path.exists() { "available" } else { "not available" },
        session_path.display()
    ));

    let root = current_download_root(app);
    app.push_message(format!(
        "Download folder: {} | overwrite: {} | custom: {}",
        root.display(),
        if app.config.download_overwrite { "on" } else { "off" },
        if app.config.download_dir.as_deref().filter(|value| !value.trim().is_empty()).is_some() { "yes" } else { "no" }
    ));
    if check_download_write {
        match check_download_root_writable(&root) {
            Ok(()) => app.push_message("Download folder write test: ok.".to_string()),
            Err(error) => app.push_message(format!("Download folder write test failed: {}", error)),
        }
    } else {
        app.push_message("Use doctor downloads to test whether the download folder is writable.".to_string());
    }

    let genre_total: usize = app.genre_cache.values().map(|items| items.len()).sum();
    let playlist_total: usize = app.playlist_cache.values().map(|items| items.len()).sum();
    let artist_total: usize = app.artist_cache.values().map(|items| items.len()).sum();
    app.push_message(format!(
        "Wildcard cache: {} genres, {} playlists, {} artists across {} server cache bucket(s).",
        genre_total,
        playlist_total,
        artist_total,
        app.genre_cache.len().max(app.playlist_cache.len()).max(app.artist_cache.len())
    ));

    app.push_message(format!(
        "Style: {} | pmc: {} | rmc: {} | custom styles: {} | file: {}",
        app.theme().label(),
        if app.config.playlist_multicolour { "on" } else { "off" },
        if app.config.result_multicolour { "on" } else { "off" },
        app.custom_styles.len(),
        app.store.custom_styles_path().display()
    ));
    if let Some(error) = &app.custom_styles_error {
        app.push_message(format!("Custom style warning: {}", error));
    }
    app.push_message(format!(
        "Media controls: setting {} | active {}{}",
        on_off(app.config.media_keys_enabled),
        if app.media_controls.is_some() { "yes" } else { "no" },
        app.media_controls_error.as_ref().map(|e| format!(" | warning: {}", e)).unwrap_or_default()
    ));

    let active_list = match app.selection_context {
        Some(SelectionContext::Results) => app.results.as_ref().map(|r| format!("results: {} (page {}/{})", r.title, r.current_page_number(), r.page_count())).unwrap_or_else(|| "results".to_string()),
        Some(SelectionContext::Queue) => format!("queue page {}/{}", app.queue_current_page_number(), app.queue_page_count()),
        Some(SelectionContext::SavedQueues) => app.saved_queues.as_ref().map(|s| format!("saved queues page {}/{}", s.current_page_number(), s.page_count())).unwrap_or_else(|| "saved queues".to_string()),
        Some(SelectionContext::Recent) => format!("playback-history page {}/{}", app.recent_current_page_number(), app.recent_page_count()),
        Some(SelectionContext::Messages) => format!("message log ({} message(s))", app.messages.len()),
        Some(SelectionContext::Help) => app.help.as_ref().map(|h| format!("command reference page {}/{}", h.current_page_number(), h.page_count())).unwrap_or_else(|| "command reference".to_string()),
        None => "none".to_string(),
    };
    app.push_message(format!("Active list context: {}", active_list));
    app.push_message("Doctor complete. Use doctor ping to test server connectivity.".to_string());
    Ok(())
}

async fn handle_doctor_ping_command(app: &mut AppState) {
    let servers = app.config.servers.clone();
    if servers.is_empty() {
        app.push_message("No servers configured to ping.".to_string());
        return;
    }

    app.push_message(format!("Pinging {} configured server(s)...", servers.len()));
    for server in servers {
        let label = format!("{} [{}]", server.name, server.alias);
        let client = SubsonicClient::new(server);
        match client.ping().await {
            Ok(message) => app.push_message(format!("Ping ok: {} - {}", label, message)),
            Err(error) => app.push_message(format!("Ping failed: {} - {}", label, error)),
        }
    }
}

fn count_toml_files(dir: &Path) -> Result<usize> {
    if !dir.exists() {
        return Ok(0);
    }
    let mut count = 0usize;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().and_then(|ext| ext.to_str()).map(|ext| ext.eq_ignore_ascii_case("toml")).unwrap_or(false) {
            count += 1;
        }
    }
    Ok(count)
}

fn check_download_root_writable(root: &Path) -> Result<()> {
    fs::create_dir_all(root)?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or(0);
    let test_path = root.join(format!(".disc_write_test_{}.tmp", stamp));
    fs::write(&test_path, b"disc_write_test")?;
    fs::remove_file(&test_path)?;
    Ok(())
}

