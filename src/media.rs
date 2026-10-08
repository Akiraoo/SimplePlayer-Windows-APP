//! Windows System Media Transport Controls (volume flyout, lock screen, media keys).

#[derive(Clone, Copy, Debug)]
pub enum MediaKey {
    Play,
    Pause,
    Toggle,
    Next,
    Previous,
    Stop,
}

#[cfg(windows)]
mod imp {
    use super::MediaKey;
    use souvlaki::{
        MediaControlEvent, MediaControls, MediaMetadata, MediaPlayback, MediaPosition,
        PlatformConfig,
    };
    use std::time::Duration;

    pub struct Media {
        controls: MediaControls,
    }

    impl Media {
        pub fn new(
            hwnd: *mut std::ffi::c_void,
            on_key: impl Fn(MediaKey) + Send + 'static,
        ) -> Option<Media> {
            let config = PlatformConfig {
                dbus_name: "simpleplayer",
                display_name: "Simple Player",
                hwnd: Some(hwnd),
            };
            let mut controls = MediaControls::new(config).ok()?;
            controls
                .attach(move |e: MediaControlEvent| {
                    let key = match e {
                        MediaControlEvent::Play => MediaKey::Play,
                        MediaControlEvent::Pause => MediaKey::Pause,
                        MediaControlEvent::Toggle => MediaKey::Toggle,
                        MediaControlEvent::Next => MediaKey::Next,
                        MediaControlEvent::Previous => MediaKey::Previous,
                        MediaControlEvent::Stop => MediaKey::Stop,
                        _ => return,
                    };
                    on_key(key);
                })
                .ok()?;
            Some(Media { controls })
        }

        pub fn set_track(
            &mut self,
            title: &str,
            artist: &str,
            album: &str,
            cover: Option<&str>,
            duration: f64,
        ) {
            let _ = self.controls.set_metadata(MediaMetadata {
                title: Some(title),
                artist: Some(artist),
                album: Some(album),
                cover_url: cover,
                duration: (duration > 0.0).then(|| Duration::from_secs_f64(duration)),
            });
        }

        pub fn set_state(&mut self, playing: bool, position: f64) {
            let progress = Some(MediaPosition(Duration::from_secs_f64(position.max(0.0))));
            let _ = self.controls.set_playback(if playing {
                MediaPlayback::Playing { progress }
            } else {
                MediaPlayback::Paused { progress }
            });
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::MediaKey;

    pub struct Media;

    impl Media {
        pub fn new(
            _hwnd: *mut std::ffi::c_void,
            _on_key: impl Fn(MediaKey) + Send + 'static,
        ) -> Option<Media> {
            None
        }
        pub fn set_track(&mut self, _: &str, _: &str, _: &str, _: Option<&str>, _: f64) {}
        pub fn set_state(&mut self, _: bool, _: f64) {}
    }
}

pub use imp::Media;
