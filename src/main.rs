use teloxide::{prelude::*, utils::command::BotCommands, respond};
use libsystemd::daemon::{self, NotifyState};
use log::{debug, info, warn, error};
use std::time::Duration;
use clap::{Parser, ArgAction};
use rodio::{Decoder, OutputStream, Sink};
use yt_dlp::Youtube;
use std::path::PathBuf;
use std::io::Cursor;
use std::sync::Arc;
use tokio::sync::Mutex;
use sha3::{Sha3_256, Digest};

// Global state for audio playback
struct AudioState {
    sink: Option<Sink>,
    _stream: Option<OutputStream>,
}

impl AudioState {
    fn new() -> Result<Self, String> {
        let stream_handle = rodio::OutputStreamBuilder::open_default_stream()
            .map_err(|e| format!("Failed to open default audio stream: {}", e))?;

        let sink = rodio::Sink::connect_new(&stream_handle.mixer());

        Ok(Self {
            sink: Some(sink),
            _stream: Some(stream_handle),
        })
    }

    fn play_audio(&self, data: Vec<u8>) -> Result<(), String> {
        if let Some(sink) = &self.sink {
            let cursor = Cursor::new(data);
            let source = Decoder::new(cursor).map_err(|e| e.to_string())?;
            sink.append(source);
            Ok(())
        } else {
            Err("Audio output not available".to_string())
        }
    }

    fn stop(&self) {
        if let Some(sink) = &self.sink {
            sink.stop();
        }
    }

    fn pause(&self) {
        if let Some(sink) = &self.sink {
            sink.pause();
        }
    }

    fn resume(&self) {
        if let Some(sink) = &self.sink {
            sink.play();
        }
    }

    fn clear(&self) {
        if let Some(sink) = &self.sink {
            sink.clear();
        }
    }

    fn is_empty(&self) -> bool {
        self.sink.as_ref().map(|s| s.empty()).unwrap_or(true)
    }

    fn len(&self) -> usize {
        self.sink.as_ref().map(|s| s.len()).unwrap_or(0)
    }
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let args = Args::parse();

    if args.systemd && !daemon::booted() {
        panic!("Not running systemd, early exit");
    }

    info!("Starting further-ng...");

    let bot = Bot::from_env();
    let audio_state = Arc::new(Mutex::new(AudioState::new().expect("Failed to initialize audio state")));

    check_state(bot.clone(), NotifyState::Ready, args.clone()).await;
    info!("Bot is connected to Telegram API");

    let watchdog_task = tokio::spawn(watchdog(bot.clone(), args.clone()));
    let repl_task = {
        let handler = Update::filter_message().endpoint(|bot: Bot, msg: Message, audio_state: Arc<Mutex<AudioState>>| async move {

            let cmd = FasterCommand::parse(msg.text().unwrap_or(""), bot.get_me().await.unwrap().username()).unwrap();
            match cmd {
                FasterCommand::Help => {
                    bot.send_message(msg.chat.id, FasterCommand::descriptions().to_string()).await?;
                }
                FasterCommand::Queue => {
                    let state = audio_state.lock().await;
                    if state.is_empty() {
                        bot.send_message(msg.chat.id, "Queue is empty").await?;
                    } else {
                        bot.send_message(msg.chat.id, format!("Queue length: {}", state.len())).await?;
                    }
                }
                FasterCommand::Play(url) => {
                    bot.send_message(msg.chat.id, "Downloading audio...").await?;
                    
                    match download_audio(&url).await {
                        Ok(audio_data) => {
                            let state = audio_state.lock().await;
                            match state.play_audio(audio_data) {
                                Ok(()) => {
                                    bot.send_message(msg.chat.id, "Now playing!").await?;
                                }
                                Err(e) => {
                                    bot.send_message(msg.chat.id, format!("Error playing audio: {}", e)).await?;
                                }
                            }
                        }
                        Err(e) => {
                            bot.send_message(msg.chat.id, format!("Error downloading audio: {}", e)).await?;
                        }
                    }
                }
                FasterCommand::Stop => {
                    let state = audio_state.lock().await;
                    state.stop();
                    bot.send_message(msg.chat.id, "Playback stopped").await?;
                }
                FasterCommand::Pause => {
                    let state = audio_state.lock().await;
                    state.pause();
                    bot.send_message(msg.chat.id, "Playback paused").await?;
                }
                FasterCommand::Resume => {
                    let state = audio_state.lock().await;
                    state.resume();
                    bot.send_message(msg.chat.id, "Playback resumed").await?;
                }
                FasterCommand::Clear => {
                    let state = audio_state.lock().await;
                    state.clear();
                    bot.send_message(msg.chat.id, "Queue cleared").await?;
                }
                FasterCommand::Info => {
                    let state = audio_state.lock().await;
                    let info = if state.is_empty() {
                        "No audio playing".to_string()
                    } else {
                        format!("Queue length: {}", state.len())
                    };
                    bot.send_message(msg.chat.id, info).await?;
                }
            };

            respond(())
        });
        
        tokio::spawn(async move {
            Dispatcher::builder(bot, handler)
                .dependencies(dptree::deps![audio_state])
                .enable_ctrlc_handler()
                .build()
                .dispatch()
                .await;
        })
    };
    
    tokio::select! {
        _ = watchdog_task => {
            debug!("watchdog_task has finished");
            report_state(NotifyState::Stopping, args).await;
        }
        _ = repl_task => {
            debug!("repl_task has finished");
            report_state(NotifyState::Stopping, args).await;
        }
    }

    info!("Main program is exiting.");
}

async fn report_state(state: NotifyState, args: Args) {
    if !args.systemd {
        return;
    }
    match daemon::notify(true, &[state.clone()]) {
        Ok(true) => {},
        Ok(false) => warn!("Systemd not notified: {}?", state),
        Err(e) => warn!("Failed to notify systemd: {}: {}", state, e),
    }
}

async fn check_state(bot: Bot, state: NotifyState, args: Args) {
    match bot.get_me().await {
        Ok(_) => report_state(state, args).await,
        Err(e) => {
            error!("Failed to connect to Telegram API: {}", e);
            report_state(NotifyState::Errno(1), args).await;
            panic!();
        }
    }
}

async fn watchdog(bot: Bot, args: Args) {
    let watchdog_duration = if args.systemd {
        daemon::watchdog_enabled(false).unwrap_or_else(|| Duration::MAX)
    } else {
        Duration::MAX
    };

    let mut watchdog_timer = tokio::time::interval(watchdog_duration);

    loop {
        watchdog_timer.tick().await;
        check_state(bot.clone(), NotifyState::Watchdog, args.clone()).await;
    }
}

#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
struct Args {
    #[clap(long = "no-systemd", action=ArgAction::SetFalse)]
    systemd: bool,
}

#[derive(BotCommands, Debug, Clone)]
#[command(rename_rule = "lowercase")]
enum FasterCommand {
    #[command(aliases = ["h", "?"])]
    Help,
    #[command(alias = "q")]
    Queue,
    #[command(alias = "p")]
    Play(String),
    #[command(alias = "s")]
    Stop,
    #[command(alias = "pa")]
    Pause,
    #[command(alias = "r")]
    Resume,
    #[command(alias = "c")]
    Clear,
    #[command(alias = "i")]
    Info,
}

async fn download_audio(url: &str) -> Result<Vec<u8>, String> {
    let url = url.to_string();
    
    // Use spawn_blocking to isolate the non-Send operations
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Runtime::new()
            .expect("Failed to create runtime")
            .block_on(async move {
                let executables_dir = PathBuf::from("libs");
                let output_dir = PathBuf::from("output");
                let fetcher = Youtube::with_new_binaries(executables_dir, output_dir)
                    .await
                    .map_err(|e| e.to_string())?;

                // Create hash for the URL
                let mut hasher = Sha3_256::default();
                hasher.update(&url);
                let url_hash = hex::encode(&hasher.finalize()[..]);

                // Download video
                let video_path = fetcher
                    .download_video_from_url(url, format!("{url_hash}.mp4"))
                    .await
                    .map_err(|e| e.to_string())?;

                // Extract audio from the video using ffmpeg
                let audio_path = format!("{url_hash}.mp3");

                let status = std::process::Command::new("ffmpeg")
                    .args(&["-i", video_path.to_str().expect("non utf-8 characters in path"), "-q:a", "0", "-map", "a", &audio_path])
                    .status()
                    .map_err(|e| e.to_string())?;

                if !status.success() {
                    return Err("Failed to extract audio from video".to_string());
                }

                // Read the audio file into memory
                let audio_data = std::fs::read(&audio_path).map_err(|e| e.to_string())?;

                // Clean up temporary files
                let _ = std::fs::remove_file(video_path);
                let _ = std::fs::remove_file(audio_path);

                Ok(audio_data)
            })
    })
    .await
    .map_err(|e| e.to_string())?
}
