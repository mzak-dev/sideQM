//! Icon decoding, off the event loop.
//!
//! Decoding a jumbo shell icon or rasterizing an SVG is slow enough to be felt
//! in the Menu's entrance animation, and the Popover used to re-extract on
//! every keystroke. So the main thread only ever builds keys and uploads
//! finished pixels; a single worker does the work and posts results back
//! through the same `EventLoopProxy` the mouse hook already uses.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::mpsc::{self, Sender};
use std::time::SystemTime;

use winit::event_loop::EventLoopProxy;

use crate::AppEvent;
use crate::config::Item;
use crate::icons::{self, IconSpec, RgbaIcon};
use crate::log;

/// A spec plus the source file's mtime: editing an icon file and reopening the
/// Menu re-decodes it, while everything else is served from memory.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub struct IconKey {
    pub spec: IconSpec,
    pub mtime: Option<SystemTime>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JobClass {
    /// A Menu Tile. Every one of them matters.
    Menu,
    /// The Popover's live preview. Only the newest is worth decoding.
    Preview,
}

#[derive(Clone, Debug)]
struct Job {
    key: IconKey,
    class: JobClass,
}

/// A finished decode. `icon: None` means it failed — already logged.
#[derive(Debug)]
pub struct IconReady {
    pub key: IconKey,
    pub icon: Option<Arc<RgbaIcon>>,
}

pub struct IconService {
    tx: Sender<Job>,
    /// `Some(icon)` decoded, `None` known-bad. Caching failures is what stops a
    /// broken path from being re-decoded on every config reload.
    // ponytail: unbounded, but entries are one key each and a session's worth
    // of typing is a few hundred; add an LRU if that ever stops being true.
    cache: HashMap<IconKey, Option<Arc<RgbaIcon>>>,
    pending: HashSet<IconKey>,
}

impl IconService {
    pub fn new(proxy: EventLoopProxy<AppEvent>) -> IconService {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("sideqm-icons".into())
            .spawn(move || worker(rx, proxy))
            .expect("icon worker thread");
        IconService {
            tx,
            cache: HashMap::new(),
            pending: HashSet::new(),
        }
    }

    /// `None` — not ready; show the fallback letter and wait for the event.
    /// `Some(Some(icon))` — cached, upload it now.
    /// `Some(None)` — known failure; the letter is the final answer.
    pub fn request(&mut self, key: IconKey, class: JobClass) -> Option<Option<Arc<RgbaIcon>>> {
        if let Some(hit) = self.cache.get(&key) {
            return Some(hit.clone());
        }
        if self.pending.insert(key.clone()) {
            let _ = self.tx.send(Job { key, class });
        }
        None
    }

    pub fn complete(&mut self, ready: &IconReady) {
        self.pending.remove(&ready.key);
        self.cache.insert(ready.key.clone(), ready.icon.clone());
    }

    pub fn key_for_item(item: &Item) -> IconKey {
        Self::key(item.icon.clone(), item.target.clone())
    }

    /// None when there is nothing to show yet (both inputs empty).
    pub fn key_for_popover(target: &str, icon_override: Option<&str>) -> Option<IconKey> {
        let target = target.trim();
        if target.is_empty() && icon_override.is_none() {
            return None;
        }
        Some(Self::key(
            icon_override.map(str::to_string),
            target.to_string(),
        ))
    }

    fn key(icon_path: Option<String>, target: String) -> IconKey {
        let probe = icon_path.clone().unwrap_or_else(|| target.clone());
        let mtime = std::fs::metadata(&probe).and_then(|m| m.modified()).ok();
        IconKey {
            spec: IconSpec { icon_path, target },
            mtime,
        }
    }
}

fn worker(rx: mpsc::Receiver<Job>, proxy: EventLoopProxy<AppEvent>) {
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    // SHGetImageList and friends need an apartment on this thread.
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(job) = rx.try_recv() {
            batch.push(job);
        }
        coalesce(&mut batch);
        for job in batch {
            let icon = match icons::load(&job.key.spec) {
                Ok(icon) => Some(Arc::new(icon)),
                Err(e) => {
                    match &job.key.spec.icon_path {
                        Some(p) => log!("could not load icon {p}: {e}"),
                        None => log!(
                            "no icon for target {}: {e}",
                            job.key.spec.target
                        ),
                    }
                    None
                }
            };
            if proxy
                .send_event(AppEvent::Icon(IconReady {
                    key: job.key,
                    icon,
                }))
                .is_err()
            {
                return; // event loop is gone; so are we
            }
        }
    }
}

/// Every Menu job is wanted; only the newest Preview is. While one decode runs,
/// a burst of keystrokes queues up behind it and all but the last are dead on
/// arrival — dropping them here is the debounce.
fn coalesce(batch: &mut Vec<Job>) {
    let Some(last_preview) = batch
        .iter()
        .rposition(|j| j.class == JobClass::Preview)
    else {
        return;
    };
    let mut i = 0;
    batch.retain(|j| {
        let keep = j.class == JobClass::Menu || i == last_preview;
        i += 1;
        keep
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(class: JobClass, target: &str) -> Job {
        Job {
            key: IconKey {
                spec: IconSpec {
                    icon_path: None,
                    target: target.into(),
                },
                mtime: None,
            },
            class,
        }
    }

    #[test]
    fn coalesce_keeps_every_menu_job_and_only_the_last_preview() {
        let mut batch = vec![
            job(JobClass::Preview, "p1"),
            job(JobClass::Menu, "m1"),
            job(JobClass::Preview, "p2"),
            job(JobClass::Menu, "m2"),
            job(JobClass::Preview, "p3"),
        ];
        coalesce(&mut batch);
        let targets: Vec<_> = batch.iter().map(|j| j.key.spec.target.as_str()).collect();
        assert_eq!(targets, ["m1", "m2", "p3"]);
    }

    #[test]
    fn coalesce_leaves_a_menu_only_batch_alone() {
        let mut batch = vec![job(JobClass::Menu, "a"), job(JobClass::Menu, "b")];
        coalesce(&mut batch);
        assert_eq!(batch.len(), 2);
    }

    #[test]
    fn popover_key_is_none_only_when_there_is_nothing_to_show() {
        assert!(IconService::key_for_popover("", None).is_none());
        assert!(IconService::key_for_popover("   ", None).is_none());
        assert!(IconService::key_for_popover("", Some("a.png")).is_some());
        assert!(IconService::key_for_popover("notepad", None).is_some());
    }

    #[test]
    fn an_explicit_icon_path_takes_precedence_over_the_target() {
        let with_icon = IconService::key_for_item(&Item {
            name: "x".into(),
            target: "notepad.exe".into(),
            icon: Some("a.png".into()),
        });
        let without = IconService::key_for_item(&Item {
            name: "x".into(),
            target: "notepad.exe".into(),
            icon: None,
        });
        assert_eq!(with_icon.spec.icon_path.as_deref(), Some("a.png"));
        assert!(without.spec.icon_path.is_none());
        assert_ne!(with_icon, without);
    }

    #[test]
    fn touching_the_source_file_changes_the_key() {
        let dir = std::env::temp_dir().join(format!("sideqm-test-key-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("icon.png");
        std::fs::write(&path, b"one").unwrap();

        let before = IconService::key_for_popover("", Some(path.to_str().unwrap())).unwrap();
        assert!(before.mtime.is_some(), "an existing file must carry an mtime");

        // Filesystem timestamps are coarse; force a distinct one.
        std::thread::sleep(std::time::Duration::from_millis(20));
        let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        file.set_modified(SystemTime::now() + std::time::Duration::from_secs(5))
            .unwrap();
        drop(file);

        let after = IconService::key_for_popover("", Some(path.to_str().unwrap())).unwrap();
        assert_ne!(before, after);
        assert_eq!(before.spec, after.spec, "only the mtime should differ");

        std::fs::remove_dir_all(&dir).ok();
    }
}
