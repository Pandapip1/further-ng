use teloxide::{prelude::*, utils::command::BotCommands, respond};
use log::{debug, info, error};
use std::time::{Duration, SystemTime};
use clap::{Parser, ArgAction};
use yt_dlp::Youtube;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use sha3::{Sha3_256, Digest};
use id3::Tag;
use id3::TagLike;
use std::fs::File;
use teloxide::types::MessageId;
use rodio::{Decoder, Source};

mod audio_queue;
use audio_queue::{AudioQueue, QueueItem};

mod systemd_utils;
use systemd_utils::{SystemdNotifier, SystemdState, watchdog};

// Set up event handling for when audio finishes
fn setup_audio_completion_handler(audio_queue: Arc<Mutex<AudioQueue>>, _bot: Bot) {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        
        loop {
            interval.tick().await;
            
            let should_play_next = {
                let queue = audio_queue.lock().await;
                queue.is_playing() && queue.sink_ran_out()
            };
            
            if should_play_next {
                let next_item = {
                    let mut queue = audio_queue.lock().await;
                    queue.play_next()
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
    let systemd_notifier = SystemdNotifier::new(args.systemd);

    if args.systemd && !systemd_notifier.is_booted() {
        panic!("Not running systemd, early exit");
    }

    info!("Starting further-ng...");

    let bot = Bot::from_env();
    let audio_queue = Arc::new(Mutex::new(AudioQueue::new().expect("Failed to initialize audio queue")));

    // Set up audio completion handler
    setup_audio_completion_handler(audio_queue.clone(), bot.clone());

    systemd_notifier.check_state(bot.clone(), SystemdState::Ready).await;
    info!("Bot is connected to Telegram API");

    let watchdog_task = tokio::spawn(watchdog(bot.clone(), systemd_notifier.clone()));
    let repl_task = {
        let handler = Update::filter_message().endpoint(|bot: Bot, msg: Message, audio_queue: Arc<Mutex<AudioQueue>>| async move {
            let username = msg.from.clone().map(|u| u.username.clone().unwrap_or_else(|| u.first_name.clone())).unwrap_or_else(|| "Unknown".to_string());

            let cmd = FasterCommand::parse(msg.text().unwrap_or(""), bot.get_me().await?.username()).unwrap();
            match cmd {
                FasterCommand::Help => {
                    bot.send_message(msg.chat.id, FasterCommand::descriptions().to_string()).await?;
                }
                FasterCommand::Queue => {
                    let queue = audio_queue.lock().await;
                    bot.send_message(msg.chat.id, queue.get_queue_info()).await?;
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
                            let mut queue = audio_queue.lock().await;
                            
                            let item = QueueItem {
                                url: url.clone(),
                                title,
                                duration,
                                added_by: username,
                                added_at: SystemTime::now(),
                                audio_data: audio_data.clone(),
                            };
                            
                            let position = queue.add_to_queue(item);
                            
                            // If nothing is currently playing, start playback
                            if !queue.is_playing() {
                                match queue.play_next() {
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
                    let mut queue = audio_queue.lock().await;
                    queue.stop();
                    bot.send_message(msg.chat.id, "Playback stopped").await?;
                }
                FasterCommand::Pause => {
                    let queue = audio_queue.lock().await;
                    queue.pause();
                    bot.send_message(msg.chat.id, "Playback paused").await?;
                }
                FasterCommand::Resume => {
                    let queue = audio_queue.lock().await;
                    queue.resume();
                    bot.send_message(msg.chat.id, "Playback resumed").await?;
                }
                FasterCommand::Clear => {
                    let mut queue = audio_queue.lock().await;
                    queue.clear_queue();
                    bot.send_message(msg.chat.id, "Queue cleared").await?;
                }
                FasterCommand::Skip => {
                    let mut queue = audio_queue.lock().await;
                    match queue.skip_current() {
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
                    let mut queue = audio_queue.lock().await;
                    if index == 0 || index > queue.queue_length() {
                        bot.send_message(msg.chat.id, "Invalid queue position").await?;
                    } else {
                        if let Some(removed_item) = queue.remove_from_queue(index - 1) {
                            bot.send_message(msg.chat.id, format!("Removed: {}", removed_item.title)).await?;
                        } else {
                            bot.send_message(msg.chat.id, "Failed to remove item").await?;
                        }
                    }
                }
                FasterCommand::NowPlaying => {
                    let queue = audio_queue.lock().await;
                    if let Some(item) = queue.get_current_item() {
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
                .dependencies(dptree::deps![audio_queue])
                .enable_ctrlc_handler()
                .build()
                .dispatch()
                .await;
        })
    };
    
    tokio::select! {
        _ = watchdog_task => {
            debug!("watchdog_task has finished");
            systemd_notifier.report_state(SystemdState::Stopping).await;
        }
        _ = repl_task => {
            debug!("repl_task has finished");
            systemd_notifier.report_state(SystemdState::Stopping).await;
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

#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
struct Args {
    #[clap(long = "no-systemd", action=ArgAction::SetFalse)]
    systemd: bool,
}
