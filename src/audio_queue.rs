use rodio::{Decoder, OutputStream, Sink};
use serde::{Serialize, Deserialize};
use std::time::Duration;
use std::io::Cursor;
use std::time::SystemTime;

// Queue item structure
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueItem {
    pub url: String,
    pub title: String,
    pub duration: Option<Duration>,
    pub added_by: String,
    pub added_at: SystemTime,
    pub audio_data: Vec<u8>,
}

// Enhanced audio state with queue management
pub struct AudioQueue {
    sink: Option<Sink>,
    _stream: Option<OutputStream>,
    queue: Vec<QueueItem>,
    is_playing: bool,
}

impl AudioQueue {
    pub fn new() -> Result<Self, String> {
        let stream_handle = rodio::OutputStreamBuilder::open_default_stream()
            .map_err(|e| format!("Failed to open default audio stream: {}", e))?;

        let sink = rodio::Sink::connect_new(&stream_handle.mixer());

        Ok(Self {
            sink: Some(sink),
            _stream: Some(stream_handle),
            queue: Vec::new(),
            is_playing: false,
        })
    }

    pub fn add_to_queue(&mut self, item: QueueItem) -> usize {
        self.queue.push(item);
        self.queue.len()
    }

    pub fn play_next(&mut self) -> Result<Option<QueueItem>, String> {
        if self.queue.is_empty() {
            self.stop();
            self.is_playing = false;
            return Ok(None);
        } else {
            self.queue.remove(0);
            if let Some(item) = self.queue.get(0) {
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

    pub fn stop(&mut self) {
        if let Some(sink) = &self.sink {
            sink.stop();
        }
        self.is_playing = false;
    }

    pub fn pause(&self) {
        if let Some(sink) = &self.sink {
            sink.pause();
        }
    }

    pub fn resume(&self) {
        if let Some(sink) = &self.sink {
            sink.play();
        }
    }

    pub fn clear_queue(&mut self) {
        self.queue.clear();
        self.stop();
    }

    pub fn remove_from_queue(&mut self, index: usize) -> Option<QueueItem> {
        if index < self.queue.len() {
            // Adjust current index if needed
            if index == 0 {
                let old_itm = self.queue.get(0);
                if let Some(item) = old_itm {
                    let itm_clone = item.clone();
                    let _ = self.play_next();
                    Some(itm_clone)
                } else {
                    None
                }
            } else {
                Some(self.queue.remove(index))
            }
        } else {
            None
        }
    }

    pub fn skip_current(&mut self) -> Result<Option<QueueItem>, String> {
        self.play_next()
    }

    // TODO: Move out of audio_queue
    pub fn get_queue_info(&self) -> String {
        if self.queue.is_empty() {
            return "Queue is empty".to_string();
        }

        let mut info = format!("Queue ({} items):\n", self.queue.len());
        
        for (i, item) in self.queue.iter().enumerate() {
            let current_indicator = if Some(i) == Some(0) { "▶ " } else { "  " };
            let duration_str = item.duration
                .map(|d| format!("[{:02}:{:02}]", d.as_secs() / 60, d.as_secs() % 60))
                .unwrap_or_else(|| "[??:??]".to_string());
            
            info.push_str(&format!("{}{}. {} {} - added by {}\n", 
                current_indicator, i + 1, duration_str, item.title, item.added_by));
        }
        
        info
    }

    pub fn get_current_item(&self) -> Option<&QueueItem> {
        self.queue.get(0)
    }

    pub fn is_playing(&self) -> bool {
        self.is_playing
    }

    pub fn queue_length(&self) -> usize {
        self.queue.len()
    }

    pub fn sink_ran_out(&self) -> bool {
        self.sink.as_ref().map(|s| s.empty()).unwrap_or(true)
    }
}
