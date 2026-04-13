use log::{debug, error, info, warn};
use rodio::Source;
use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::sync::mpsc::{RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;
use tauri::{AppHandle, Manager};

// How long to keep the audio output stream alive after the last sound request.
// Holding the stream across idle time wakes a WASAPI worker on every buffer
// period on Windows (~108 wakes/sec for the 10 ms shared-mode default), which
// showed up as ~8% sustained idle CPU on one core (#290). Start/stop sound
// pairs fire well within this window, so both play on one open stream before
// it is released.
const STREAM_KEEP_ALIVE: Duration = Duration::from_secs(10);

pub enum Sound {
    StartRecording,
    StopRecording,
}

impl Sound {
    fn filename(&self) -> &'static str {
        match self {
            Sound::StartRecording => "start_record.mp3",
            Sound::StopRecording => "stop_record.mp3",
        }
    }
}

pub struct SoundManager {
    tx: Sender<Sound>,
}

fn resolve_sound_path(app: &AppHandle, filename: &str) -> Option<PathBuf> {
    crate::utils::resources::resolve_resource_path(app, &format!("audio/{}", filename))
}

fn load_sound_bytes(app: &AppHandle, filename: &str) -> Option<Vec<u8>> {
    if let Some(path) = resolve_sound_path(app, filename) {
        if let Ok(mut file) = File::open(&path) {
            let mut buffer = Vec::new();
            if file.read_to_end(&mut buffer).is_ok() {
                debug!("Loaded sound: {:?}", path);
                return Some(buffer);
            }
        }
    }
    warn!("Failed to load sound: {}", filename);
    None
}

pub fn init_sound_system(app: &AppHandle) {
    let (tx, rx) = std::sync::mpsc::channel::<Sound>();
    let app_handle = app.clone();

    thread::spawn(move || {
        // Preload sound bytes once; decoding and playback happen lazily.
        let mut sound_cache: HashMap<&'static str, Option<Vec<u8>>> = HashMap::new();
        sound_cache.insert(
            Sound::StartRecording.filename(),
            load_sound_bytes(&app_handle, Sound::StartRecording.filename()),
        );
        sound_cache.insert(
            Sound::StopRecording.filename(),
            load_sound_bytes(&app_handle, Sound::StopRecording.filename()),
        );

        loop {
            // No stream held: block until the next sound request (or shutdown).
            let first = match rx.recv() {
                Ok(s) => s,
                Err(_) => return,
            };

            // Lazy-open the output stream with fallback for macOS compatibility.
            let mut stream_handle = match rodio::DeviceSinkBuilder::from_default_device() {
                Ok(builder) => match builder.open_sink_or_fallback() {
                    Ok(stream) => stream,
                    Err(e) => {
                        error!("Failed to open audio output stream: {}", e);
                        continue;
                    }
                },
                Err(e) => {
                    error!("Failed to get default audio device: {}", e);
                    continue;
                }
            };
            // We release the sink deliberately on idle timeout; don't spam stderr.
            stream_handle.log_on_drop(false);

            info!("Audio output stream initialized");

            // Warmup: silent tone to wake the audio device before decoding.
            let warmup_sink = rodio::Player::connect_new(stream_handle.mixer());
            warmup_sink.append(
                rodio::source::SineWave::new(440.0)
                    .take_duration(Duration::from_millis(10))
                    .amplify(0.0),
            );
            warmup_sink.detach();

            play_cached(&stream_handle, &sound_cache, first.filename());

            // Keep the stream alive while sounds keep arriving within the window.
            loop {
                match rx.recv_timeout(STREAM_KEEP_ALIVE) {
                    Ok(next) => play_cached(&stream_handle, &sound_cache, next.filename()),
                    Err(RecvTimeoutError::Timeout) => {
                        debug!("Audio output stream idle, releasing");
                        break;
                    }
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            // stream_handle drops here, closing the cpal::Stream and releasing
            // the WASAPI worker until the next sound request.
        }
    });

    app.manage(SoundManager { tx });
}

fn play_cached(
    stream: &rodio::MixerDeviceSink,
    cache: &HashMap<&'static str, Option<Vec<u8>>>,
    filename: &'static str,
) {
    let Some(Some(bytes)) = cache.get(filename) else {
        warn!("Sound not found in cache: {}", filename);
        return;
    };
    let cursor = std::io::Cursor::new(bytes.clone());
    match rodio::Decoder::new(cursor) {
        Ok(source) => {
            let sink = rodio::Player::connect_new(stream.mixer());
            sink.append(source);
            sink.detach();
        }
        Err(e) => error!("Failed to decode sound {}: {}", filename, e),
    }
}

pub fn play_sound(app: &AppHandle, sound: Sound) {
    if let Some(manager) = app.try_state::<SoundManager>() {
        let _ = manager.tx.send(sound);
    } else {
        warn!("SoundManager not initialized");
    }
}
