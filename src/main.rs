use teloxide::{prelude::*, utils::command::BotCommands, respond};
use libsystemd::daemon::{self, NotifyState};
use log::{debug, info, warn, error};
use std::time::{Duration, SystemTime};
use clap::{Parser, ArgAction};
use rodio::{Decoder, Source, OutputStream, Sink};
use yt_dlp::Youtube;
use std::path::PathBuf;
use std::io::Cursor;
use std::sync::Arc;
use tokio::sync::Mutex;
use sha3::{Sha3_256, Digest};
use serde::{Serialize, Deserialize};
use id3::Tag;
use id3::TagLike;
use std::fs::File;
use teloxide::types::MessageId;

// Queue item structure
#[derive(Debug, Clone, Serialize, Deserialize)]
struct QueueItem {
    url: String,
    title: String,
    duration: Option<Duration>,
    added_by: String,
    added_at: SystemTime,
    audio_data: Vec<u8>,
}

// Enhanced audio state with queue management
struct AudioState {
    sink: Option<Sink>,
    _stream: Option<OutputStream>,
    queue: Vec<QueueItem>,
    current_index: Option<usize>,
    is_playing: bool,
}

impl AudioState {
    fn new() -> Result<Self, String> {
        let stream_handle = rodio::OutputStreamBuilder::open_default_stream()
            .map_err(|e| format!("Failed to open default audio stream: {}", e))?;

        let sink = rodio::Sink::connect_new(&stream_handle.mixer());

        Ok(Self {
            sink: Some(sink),
            _stream: Some(stream_handle),
            queue: Vec::new(),
            current_index: None,
            is_playing: false,
        })
    }

    fn add_to_queue(&mut self, item: QueueItem) -> usize {
        self.queue.push(item);
        self.queue.len()
    }

    fn play_next(&mut self) -> Result<Option<QueueItem>, String> {
        // If we're at the end of the queue, stop
        if let Some(current_idx) = self.current_index {
            if current_idx + 1 >= self.queue.len() {
                self.stop();
                self.current_index = None;
                self.is_playing = false;
                return Ok(None);
            }
            
            self.current_index = Some(current_idx + 1);
        } else if !self.queue.is_empty() {
            self.current_index = Some(0);
        } else {
            return Ok(None);
        }

        if let Some(current_idx) = self.current_index {
            if let Some(item) = self.queue.get(current_idx) {
                let item_clone = item.clone();
                self.play_audio(item.audio_data.clone())?;
                self.is_playing = true;
                return Ok(Some(item_clone));
            }
        }
        
        Ok(None)
    }

    fn play_audio(&mut self, data: Vec<u8>) -> Result<(), String> {
        if let Some(sink) = &self.sink {
            let cursor = Cursor::new(data);
            let source = Decoder::new(cursor).map_err(|e| e.to_string())?;
            
            // Clear any existing audio and play new one
            sink.stop();
            sink.append(source);
            Ok(())
        } else {
            Err("Audio output not available".to_string())
        }
    }

    fn stop(&mut self) {
        if let Some(sink) = &self.sink {
            sink.stop();
        }
        self.is_playing = false;
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

    fn clear_queue(&mut self) {
        self.queue.clear();
        self.stop();
        self.current_index = None;
    }

    fn remove_from_queue(&mut self, index: usize) -> Option<QueueItem> {
        if index < self.queue.len() {
            let item = self.queue.remove(index);
            
            // Adjust current index if needed
            if let Some(current_idx) = self.current_index {
                if index == current_idx {
                    // Currently playing item was removed
                    self.stop();
                    self.current_index = None;
                } else if index < current_idx {
                    self.current_index = Some(current_idx - 1);
                }
            }
            
            Some(item)
        } else {
            None
        }
    }

    fn skip_current(&mut self) -> Result<Option<QueueItem>, String> {
        self.play_next()
    }

    fn get_queue_info(&self) -> String {
        if self.queue.is_empty() {
            return "Queue is empty".to_string();
        }

        let mut info = format!("Queue ({} items):\n", self.queue.len());
        
        for (i, item) in self.queue.iter().enumerate() {
            let current_indicator = if Some(i) == self.current_index { "▶ " } else { "  " };
            let duration_str = item.duration
                .map(|d| format!("[{:02}:{:02}]", d.as_secs() / 60, d.as_secs() % 60))
                .unwrap_or_else(|| "[??:??]".to_string());
            
            info.push_str(&format!("{}{}. {} {} - added by {}\n", 
                current_indicator, i + 1, duration_str, item.title, item.added_by));
        }
        
        info
    }

    fn get_current_item(&self) -> Option<&QueueItem> {
        self.current_index.and_then(|idx| self.queue.get(idx))
    }

    fn is_playing(&self) -> bool {
        self.is_playing
    }

    fn queue_length(&self) -> usize {
        self.queue.len()
    }
}

// Set up event handling for when audio finishes
fn setup_audio_completion_handler(audio_state: Arc<Mutex<AudioState>>, _bot: Bot) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        
        loop {
            interval.tick().await;
            
            let should_play_next = {
                let state = audio_state.lock().await;
                state.is_playing() && state.sink.as_ref().map(|s| s.empty()).unwrap_or(true)
            };
            
            if should_play_next {
                let next_item = {
                    let mut state = audio_state.lock().await;
                    state.play_next()
                };
                
                if let Ok(Some(item)) = next_item {
                    // You could send a notification about the next track playing
                    debug!("Now playing: {}", item.title);
                }
            }
        }
    });
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

    // Set up audio completion handler
    setup_audio_completion_handler(audio_state.clone(), bot.clone());

    check_state(bot.clone(), NotifyState::Ready, args.clone()).await;
    info!("Bot is connected to Telegram API");

    let watchdog_task = tokio::spawn(watchdog(bot.clone(), args.clone()));
    let repl_task = {
        let handler = Update::filter_message().endpoint(|bot: Bot, msg: Message, audio_state: Arc<Mutex<AudioState>>| async move {
            let username = msg.from.clone().map(|u| u.username.clone().unwrap_or_else(|| u.first_name.clone())).unwrap_or_else(|| "Unknown".to_string());

            let cmd = FasterCommand::parse(msg.text().unwrap_or(""), bot.get_me().await?.username()).unwrap();
            match cmd {
                FasterCommand::Help => {
                    bot.send_message(msg.chat.id, FasterCommand::descriptions().to_string()).await?;
                }
                FasterCommand::Queue => {
                    let state = audio_state.lock().await;
                    bot.send_message(msg.chat.id, state.get_queue_info()).await?;
                }
                FasterCommand::Play(url) => {
                    let msg_handle = bot.send_message(msg.chat.id, "Downloading audio...").await?;

                    match download_audio_with_progress(
                        bot.clone(),
                        msg.chat.id,
                        msg_handle.id,
                        &url,
                    ).await {
                        Ok((audio_data, title, duration)) => {
                            let mut state = audio_state.lock().await;
                            
                            let item = QueueItem {
                                url: url.clone(),
                                title,
                                duration,
                                added_by: username,
                                added_at: SystemTime::now(),
                                audio_data: audio_data.clone(),
                            };
                            
                            let position = state.add_to_queue(item);
                            
                            // If nothing is currently playing, start playback
                            if !state.is_playing() {
                                match state.play_next() {
                                    Ok(Some(item)) => {
                                        bot.send_message(msg.chat.id, format!("Added to queue (#{}). Now playing: {}", position, item.title)).await?;
                                        info!("Added to queue (#{}). Now playing: {}", position, item.title);
                                    }
                                    Ok(None) => {
                                        bot.send_message(msg.chat.id, format!("Added to queue (#{})", position)).await?;
                                        info!("Added to queue (#{})", position);
                                    }
                                    Err(e) => {
                                        bot.send_message(msg.chat.id, format!("Error playing audio: {}", e)).await?;
                                        error!("Error playing audio: {}", e);
                                    }
                                }
                            } else {
                                bot.send_message(msg.chat.id, format!("Added to queue at position #{}", position)).await?;
                                info!("Added to queue at position #{}", position)
                            }
                        }
                        Err(e) => {
                            bot.edit_message_text(msg.chat.id, msg_handle.id, format!("Error: {}", e)).await?;
                        }
                    }
                }
                FasterCommand::Stop => {
                    let mut state = audio_state.lock().await;
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
                    let mut state = audio_state.lock().await;
                    state.clear_queue();
                    bot.send_message(msg.chat.id, "Queue cleared").await?;
                }
                FasterCommand::Skip => {
                    let mut state = audio_state.lock().await;
                    match state.skip_current() {
                        Ok(Some(item)) => {
                            bot.send_message(msg.chat.id, format!("Skipped! Now playing: {}", item.title)).await?;
                        }
                        Ok(None) => {
                            bot.send_message(msg.chat.id, "Skipped! Queue is now empty.").await?;
                        }
                        Err(e) => {
                            bot.send_message(msg.chat.id, format!("Error skipping: {}", e)).await?;
                        }
                    }
                }
                FasterCommand::Remove(index) => {
                    let mut state = audio_state.lock().await;
                    if index == 0 || index > state.queue_length() {
                        bot.send_message(msg.chat.id, "Invalid queue position").await?;
                    } else {
                        if let Some(removed_item) = state.remove_from_queue(index - 1) {
                            bot.send_message(msg.chat.id, format!("Removed: {}", removed_item.title)).await?;
                        } else {
                            bot.send_message(msg.chat.id, "Failed to remove item").await?;
                        }
                    }
                }
                FasterCommand::NowPlaying => {
                    let state = audio_state.lock().await;
                    if let Some(item) = state.get_current_item() {
                        let duration_str = item.duration
                            .map(|d| format!("{:02}:{:02}", d.as_secs() / 60, d.as_secs() % 60))
                            .unwrap_or_else(|| "??:??".to_string());
                        
                        bot.send_message(msg.chat.id, 
                            format!("Now Playing: {}\nDuration: {}\nAdded by: {}", 
                            item.title, duration_str, item.added_by)).await?;
                    } else {
                        bot.send_message(msg.chat.id, "Nothing is currently playing").await?;
                    }
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
    #[command(alias = "sk")]
    Skip,
    #[command(alias = "rm")]
    Remove(usize),
    #[command(alias = "np")]
    NowPlaying,
}

async fn download_audio_with_progress(
    bot: Bot,
    chat_id: ChatId,
    message_id: MessageId,
    url: &str,
) -> Result<(Vec<u8>, String, Option<Duration>), String> {
    let url = url.to_string();
    
    tokio::task::spawn_blocking(move || {
        tokio::runtime::Runtime::new()
            .expect("Failed to create runtime")
            .block_on(async move {
                let executables_dir = PathBuf::from("libs");
                let output_dir = PathBuf::from("/tmp/further-ng-cache");
                let mut fetcher = Youtube::with_new_binaries(executables_dir, output_dir)
                    .await
                    .map_err(|e| e.to_string())?;
                fetcher.with_timeout(Duration::from_secs(600));

                let mut hasher = Sha3_256::default();
                hasher.update(&url);
                let url_hash = hex::encode(&hasher.finalize()[..]);

                // Download video
                let video_path = format!("/tmp/{url_hash}.mp4");
                let video = fetcher.fetch_video_infos(url).await.expect("Failed to fetch video data");
                let bot_copy = bot.clone();
                let download_id = fetcher
                    .download_video_with_progress(&video, &video_path, move |downloaded, total| {
                        let percent = if total > 0 {
                            (downloaded as f64 / total as f64 * 100.0) as u64
                        } else {
                            0
                        };
                        let text = format!("Downloading... {:.1}% complete", percent);

                        let bot = bot_copy.clone();
                        let chat_id = chat_id;
                        let message_id = message_id;
                        tokio::spawn(async move {
                            match bot.edit_message_text(chat_id, message_id, text).await {
                                Ok(_) => {},
                                Err(e) => error!("Failed to update download percent: {}", e)
                            }
                        });
                    })
                    .await
                    .map_err(|e| e.to_string())?;
                fetcher.wait_for_download(download_id).await;

                let audio_path = format!("/tmp/{url_hash}.mp3");

                let status = std::process::Command::new("ffmpeg")
                    .args(&["-i", &video_path.to_string(), "-q:a", "0", "-map", "a", &audio_path])
                    .status()
                    .map_err(|e| e.to_string())?;

                if !status.success() {
                    return Err("Failed to extract audio from video".to_string());
                }

                // Read the audio file into a vector
                let audio_data = match std::fs::read(&audio_path) {
                    Ok(data) => data,
                    Err(e) => return Err(e.to_string()),
                };

                // Extract the title from the MP3 file using ID3 tags
                let title = match Tag::read_from_path(&audio_path) {
                    Ok(tag) => tag.title().unwrap_or_else(|| "Unknown Title").to_owned(),
                    Err(_) => "Unknown Title".to_owned(),
                };

                // Calculate the duration using rodio
                let file = match File::open(&audio_path) {
                    Ok(f) => f,
                    Err(_) => return Err("Failed to open audio file".to_string()),
                };

                let decoder = match Decoder::new(std::io::BufReader::new(file)) {
                    Ok(d) => d,
                    Err(_) => return Err("Failed to decode audio".to_string()),
                };

                let duration = decoder.total_duration();

                // Clean up downloaded files
                let _ = std::fs::remove_file(audio_path);

                let _ = bot
                    .edit_message_text(chat_id, message_id, "Download complete! Added to queue.")
                    .await;

                Ok((audio_data, title, duration))
            })
    })
    .await
    .map_err(|e| e.to_string())?
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
