use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::thread;

use crate::project::Media;

use super::{CachingSource, FrameSource, ImageBuf};

const PENDING_LIMIT: usize = 64;
const COMPLETED_LIMIT: usize = 16;
const THUMBNAIL_WIDTH: u32 = 160;
const THUMBNAIL_HEIGHT: u32 = 90;

pub struct Thumbnail {
    pub media: Media,
    pub image: Option<ImageBuf> }

#[derive(Default)]
struct ThumbnailMailbox {
    pending: VecDeque<Media>,
    completed: VecDeque<Thumbnail>,
    current: Option<Media>,
    stopped: bool }

pub struct ThumbnailEngine {
    mailbox: Arc<(Mutex<ThumbnailMailbox>, Condvar)> }

impl ThumbnailEngine {
    pub fn new() -> Self {
        let mailbox = Arc::new((Mutex::new(ThumbnailMailbox::default()), Condvar::new()));
        let worker_mailbox = mailbox.clone();
        thread::spawn(move || thumbnail_loop(worker_mailbox));
        Self { mailbox }
    }

    pub fn request(&self, media: Media) {
        let (lock, ready) = &*self.mailbox;
        let mut mailbox = lock.lock().unwrap_or_else(|err| err.into_inner());
        if mailbox.stopped {
            return;
        }

        let current_id = mailbox.current.as_ref().map(|current| current.id);
        if current_id == Some(media.id) {
            if mailbox.current.as_ref() == Some(&media) {
                return;
            }
            // The running decode keeps its original snapshot. Recording a replacement
            // here prevents it from publishing an image for a newly relinked asset.
            mailbox.current = Some(media.clone());
        }
        if mailbox.pending.iter().any(|pending| pending == &media)
            || mailbox.completed.iter().any(|done| done.media == media)
        {
            return;
        }

        mailbox.pending.retain(|pending| pending.id != media.id);
        mailbox.completed.retain(|done| done.media.id != media.id);
        if mailbox.pending.len() >= PENDING_LIMIT {
            mailbox.pending.pop_front();
        }
        mailbox.pending.push_back(media);
        ready.notify_one();
    }

    pub fn poll(&self) -> Option<Thumbnail> {
        self.mailbox.0.lock().ok()?.completed.pop_front()
    }
}

impl Default for ThumbnailEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ThumbnailEngine {
    fn drop(&mut self) {
        let (lock, ready) = &*self.mailbox;
        let mut mailbox = lock.lock().unwrap_or_else(|err| err.into_inner());
        mailbox.stopped = true;
        mailbox.pending.clear();
        mailbox.completed.clear();
        ready.notify_one();
    }
}

fn thumbnail_loop(shared: Arc<(Mutex<ThumbnailMailbox>, Condvar)>) {
    let mut source = CachingSource::new();
    loop {
        let (lock, ready) = &*shared;
        let media = {
            let mut mailbox = lock.lock().unwrap_or_else(|err| err.into_inner());
            while mailbox.pending.is_empty() && !mailbox.stopped {
                mailbox = ready.wait(mailbox).unwrap_or_else(|err| err.into_inner());
            }
            if mailbox.stopped {
                return;
            }
            let Some(media) = mailbox.pending.pop_front() else {
                continue;
            };
            mailbox.current = Some(media.clone());
            media
        };

        // Keep only the current asset registered. This limits the worker's decoded
        // picture cache to one small frame while retaining a hit for repeated requests.
        source.set_project_media(std::slice::from_ref(&media));
        let decoded = source.video(media.id, 0.0, THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT);
        let image = decoded.map(fit_thumbnail);

        let mut mailbox = lock.lock().unwrap_or_else(|err| err.into_inner());
        if mailbox.stopped {
            return;
        }
        let current = mailbox.current.take();
        if current.as_ref() != Some(&media) {
            continue;
        }
        mailbox.pending.retain(|pending| pending != &media);
        if mailbox.completed.len() >= COMPLETED_LIMIT {
            mailbox.completed.pop_front();
        }
        mailbox.completed.push_back(Thumbnail { media, image });
    }
}

fn fit_thumbnail(decoded: ImageBuf) -> ImageBuf {
    let mut image = ImageBuf::black(THUMBNAIL_WIDTH, THUMBNAIL_HEIGHT);
    let width = decoded.width.min(THUMBNAIL_WIDTH);
    let height = decoded.height.min(THUMBNAIL_HEIGHT);
    let left = (THUMBNAIL_WIDTH - width) / 2;
    let top = (THUMBNAIL_HEIGHT - height) / 2;
    let bytes = width as usize * 3;
    for row in 0..height {
        let source = row as usize * decoded.width as usize * 3;
        let target = ((top + row) as usize * THUMBNAIL_WIDTH as usize + left as usize) * 3;
        image.rgb[target..target + bytes].copy_from_slice(&decoded.rgb[source..source + bytes]);
    }
    image
}
