use libsystemd::daemon::{self, NotifyState};
use log::{warn, error};
use std::time::Duration;
use teloxide::prelude::*;

#[derive(Clone)]
pub struct SystemdNotifier {
    enabled: bool,
}

#[derive(Clone)]
pub enum SystemdState {
    Ready,
    Watchdog,
    Stopping,
    Errno(i32),
}

impl SystemdNotifier {
    pub fn new(enabled: bool) -> Self {
        Self { enabled }
    }

    pub fn is_booted(&self) -> bool {
        if !self.enabled {
            return true;
        }
        daemon::booted()
    }

    pub async fn report_state(&self, state: SystemdState) {
        if !self.enabled {
            return;
        }
        
        let notify_state = match state {
            SystemdState::Ready => NotifyState::Ready,
            SystemdState::Watchdog => NotifyState::Watchdog,
            SystemdState::Stopping => NotifyState::Stopping,
            SystemdState::Errno(errno) => NotifyState::Errno(errno.try_into().unwrap()),
        };

        match daemon::notify(true, &[notify_state.clone()]) {
            Ok(true) => {},
            Ok(false) => warn!("Systemd not notified: {}?", notify_state),
            Err(e) => warn!("Failed to notify systemd: {}: {}", notify_state, e),
        }
    }

    pub async fn check_state(&self, bot: Bot, state: SystemdState) {
        match bot.get_me().await {
            Ok(_) => self.report_state(state).await,
            Err(e) => {
                error!("Failed to connect to Telegram API: {}", e);
                self.report_state(SystemdState::Errno(1)).await;
                panic!();
            }
        }
    }

    pub fn get_watchdog_duration(&self) -> Option<Duration> {
        if !self.enabled {
            return None;
        }
        daemon::watchdog_enabled(false)
    }
}

pub async fn watchdog(bot: Bot, systemd_notifier: SystemdNotifier) {
    let watchdog_duration = if systemd_notifier.enabled {
        systemd_notifier.get_watchdog_duration().unwrap_or_else(|| Duration::MAX)
    } else {
        Duration::MAX
    };

    let mut watchdog_timer = tokio::time::interval(watchdog_duration);

    loop {
        watchdog_timer.tick().await;
        systemd_notifier.check_state(bot.clone(), SystemdState::Watchdog).await;
    }
}
