mod flags;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::LazyLock;
use std::time::Duration;
use std::{future::Future, pin::Pin, task::Poll};

use futures::channel::oneshot;
use futures::{FutureExt, select_biased};
use gpui::{App, Context, Global, Subscription, Task, Window};
use serde::Deserialize;

pub use flags::*;

#[derive(Default)]
struct FeatureFlags {
    flags: Vec<String>,
    staff: bool,
}

pub static ZED_DISABLE_STAFF: LazyLock<bool> = LazyLock::new(|| {
    std::env::var("ZED_DISABLE_STAFF").is_ok_and(|value| !value.is_empty() && value != "0")
});

impl FeatureFlags {
    fn has_flag<T: FeatureFlag>(&self) -> bool {
        if T::enabled_for_all() {
            return true;
        }

        if (cfg!(debug_assertions) || self.staff) && !*ZED_DISABLE_STAFF && T::enabled_for_staff() {
            return true;
        }

        self.flags.iter().any(|f| f.as_str() == T::NAME)
    }
}

impl Global for FeatureFlags {}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct FeatureFlagsOverrides {
    staff: Option<bool>,
    flags: HashMap<String, bool>,
}

fn feature_flags_overrides_path() -> std::path::PathBuf {
    paths::config_dir().join("feature_flags.json")
}

fn load_feature_flags_overrides() -> Option<FeatureFlagsOverrides> {
    let feature_flags_overrides_path = feature_flags_overrides_path();
    let overrides_content = match std::fs::read_to_string(&feature_flags_overrides_path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            log::error!(
                "failed to read feature flag overrides from {}: {error}",
                feature_flags_overrides_path.display(),
            );
            return None;
        }
    };

    match serde_json::from_str::<FeatureFlagsOverrides>(&overrides_content) {
        Ok(overrides) => Some(overrides),
        Err(error) => {
            log::error!(
                "failed to parse feature flag overrides from {}: {error}",
                feature_flags_overrides_path.display(),
            );
            None
        }
    }
}

fn apply_feature_flags_overrides(
    mut staff: bool,
    flags: Vec<String>,
    overrides: Option<FeatureFlagsOverrides>,
) -> (bool, Vec<String>) {
    let Some(overrides) = overrides else {
        return (staff, flags);
    };

    if let Some(staff_override) = overrides.staff {
        staff = staff_override;
    }

    let mut merged_flags: HashSet<String> = flags.into_iter().collect();
    for (flag_name, enabled) in overrides.flags {
        if enabled {
            merged_flags.insert(flag_name);
        } else {
            merged_flags.remove(&flag_name);
        }
    }

    let mut merged_flags: Vec<String> = merged_flags.into_iter().collect();
    merged_flags.sort_unstable();

    (staff, merged_flags)
}

fn set_feature_flags(staff: bool, flags: Vec<String>, cx: &mut App) {
    let feature_flags = cx.default_global::<FeatureFlags>();
    feature_flags.staff = staff;
    feature_flags.flags = flags;
}

pub fn init(cx: &mut App) {
    let Some(overrides) = load_feature_flags_overrides() else {
        return;
    };
    let (staff, flags) = apply_feature_flags_overrides(false, Vec::new(), Some(overrides));
    set_feature_flags(staff, flags, cx);
}

/// To create a feature flag, implement this trait on a trivial type and use it as
/// a generic parameter when called [`FeatureFlagAppExt::has_flag`].
///
/// Feature flags are enabled for members of Zed staff by default. To disable this behavior
/// so you can test flags being disabled, set ZED_DISABLE_STAFF=1 in your environment,
/// which will force Zed to treat the current user as non-staff.
pub trait FeatureFlag {
    const NAME: &'static str;

    /// Returns whether this feature flag is enabled for Zed staff.
    fn enabled_for_staff() -> bool {
        true
    }

    /// Returns whether this feature flag is enabled for everyone.
    ///
    /// This is generally done on the server, but we provide this as a way to entirely enable a feature flag client-side
    /// without needing to remove all of the call sites.
    fn enabled_for_all() -> bool {
        false
    }
}

pub trait FeatureFlagViewExt<V: 'static> {
    fn observe_flag<T: FeatureFlag, F>(&mut self, window: &Window, callback: F) -> Subscription
    where
        F: Fn(bool, &mut V, &mut Window, &mut Context<V>) + Send + Sync + 'static;

    fn when_flag_enabled<T: FeatureFlag>(
        &mut self,
        window: &mut Window,
        callback: impl Fn(&mut V, &mut Window, &mut Context<V>) + Send + Sync + 'static,
    );
}

impl<V> FeatureFlagViewExt<V> for Context<'_, V>
where
    V: 'static,
{
    fn observe_flag<T: FeatureFlag, F>(&mut self, window: &Window, callback: F) -> Subscription
    where
        F: Fn(bool, &mut V, &mut Window, &mut Context<V>) + 'static,
    {
        self.observe_global_in::<FeatureFlags>(window, move |v, window, cx| {
            let feature_flags = cx.global::<FeatureFlags>();
            callback(feature_flags.has_flag::<T>(), v, window, cx);
        })
    }

    fn when_flag_enabled<T: FeatureFlag>(
        &mut self,
        window: &mut Window,
        callback: impl Fn(&mut V, &mut Window, &mut Context<V>) + Send + Sync + 'static,
    ) {
        if self
            .try_global::<FeatureFlags>()
            .is_some_and(|f| f.has_flag::<T>())
        {
            self.defer_in(window, move |view, window, cx| {
                callback(view, window, cx);
            });
            return;
        }
        let subscription = Rc::new(RefCell::new(None));
        let inner = self.observe_global_in::<FeatureFlags>(window, {
            let subscription = subscription.clone();
            move |v, window, cx| {
                let feature_flags = cx.global::<FeatureFlags>();
                if feature_flags.has_flag::<T>() {
                    callback(v, window, cx);
                    subscription.take();
                }
            }
        });
        subscription.borrow_mut().replace(inner);
    }
}

#[derive(Debug)]
pub struct OnFlagsReady {
    pub is_staff: bool,
}

pub trait FeatureFlagAppExt {
    fn wait_for_flag<T: FeatureFlag>(&mut self) -> WaitForFlag;

    /// Waits for the specified feature flag to resolve, up to the given timeout.
    fn wait_for_flag_or_timeout<T: FeatureFlag>(&mut self, timeout: Duration) -> Task<bool>;

    fn update_flags(&mut self, staff: bool, flags: Vec<String>);
    fn set_staff(&mut self, staff: bool);
    fn has_flag<T: FeatureFlag>(&self) -> bool;
    fn is_staff(&self) -> bool;

    fn on_flags_ready<F>(&mut self, callback: F) -> Subscription
    where
        F: FnMut(OnFlagsReady, &mut App) + 'static;

    fn observe_flag<T: FeatureFlag, F>(&mut self, callback: F) -> Subscription
    where
        F: FnMut(bool, &mut App) + 'static;
}

impl FeatureFlagAppExt for App {
    fn update_flags(&mut self, staff: bool, flags: Vec<String>) {
        let (staff, flags) =
            apply_feature_flags_overrides(staff, flags, load_feature_flags_overrides());
        set_feature_flags(staff, flags, self);
    }

    fn set_staff(&mut self, staff: bool) {
        let feature_flags = self.default_global::<FeatureFlags>();
        feature_flags.staff = staff;
    }

    fn has_flag<T: FeatureFlag>(&self) -> bool {
        self.try_global::<FeatureFlags>()
            .map(|flags| flags.has_flag::<T>())
            .unwrap_or_else(|| {
                (cfg!(debug_assertions) && T::enabled_for_staff() && !*ZED_DISABLE_STAFF)
                    || T::enabled_for_all()
            })
    }

    fn is_staff(&self) -> bool {
        self.try_global::<FeatureFlags>()
            .map(|flags| flags.staff)
            .unwrap_or(false)
    }

    fn on_flags_ready<F>(&mut self, mut callback: F) -> Subscription
    where
        F: FnMut(OnFlagsReady, &mut App) + 'static,
    {
        self.observe_global::<FeatureFlags>(move |cx| {
            let feature_flags = cx.global::<FeatureFlags>();
            callback(
                OnFlagsReady {
                    is_staff: feature_flags.staff,
                },
                cx,
            );
        })
    }

    fn observe_flag<T: FeatureFlag, F>(&mut self, mut callback: F) -> Subscription
    where
        F: FnMut(bool, &mut App) + 'static,
    {
        self.observe_global::<FeatureFlags>(move |cx| {
            let feature_flags = cx.global::<FeatureFlags>();
            callback(feature_flags.has_flag::<T>(), cx);
        })
    }

    fn wait_for_flag<T: FeatureFlag>(&mut self) -> WaitForFlag {
        let (tx, rx) = oneshot::channel::<bool>();
        let mut tx = Some(tx);
        let subscription: Option<Subscription>;

        match self.try_global::<FeatureFlags>() {
            Some(feature_flags) => {
                subscription = None;
                tx.take().unwrap().send(feature_flags.has_flag::<T>()).ok();
            }
            None => {
                subscription = Some(self.observe_global::<FeatureFlags>(move |cx| {
                    let feature_flags = cx.global::<FeatureFlags>();
                    if let Some(tx) = tx.take() {
                        tx.send(feature_flags.has_flag::<T>()).ok();
                    }
                }));
            }
        }

        WaitForFlag(rx, subscription)
    }

    fn wait_for_flag_or_timeout<T: FeatureFlag>(&mut self, timeout: Duration) -> Task<bool> {
        let wait_for_flag = self.wait_for_flag::<T>();

        self.spawn(async move |cx| {
            let mut wait_for_flag = wait_for_flag.fuse();
            let mut timeout = FutureExt::fuse(cx.background_executor().timer(timeout));

            select_biased! {
                is_enabled = wait_for_flag => is_enabled,
                _ = timeout => false,
            }
        })
    }
}

pub struct WaitForFlag(oneshot::Receiver<bool>, Option<Subscription>);

impl Future for WaitForFlag {
    type Output = bool;

    fn poll(mut self: Pin<&mut Self>, cx: &mut core::task::Context<'_>) -> Poll<Self::Output> {
        self.0.poll_unpin(cx).map(|result| {
            self.1.take();
            result.unwrap_or(false)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{FeatureFlagsOverrides, apply_feature_flags_overrides};
    use std::collections::HashMap;

    #[test]
    fn preserves_server_values_without_local_overrides() {
        let (staff, flags) =
            apply_feature_flags_overrides(false, vec!["server-flag".to_string()], None);

        assert!(!staff);
        assert_eq!(flags, vec!["server-flag".to_string()]);
    }

    #[test]
    fn local_overrides_take_priority_over_server_values() {
        let mut overridden_flags = HashMap::default();
        overridden_flags.insert("server-only".to_string(), false);
        overridden_flags.insert("local-only".to_string(), true);
        overridden_flags.insert("split-diff".to_string(), true);

        let overrides = FeatureFlagsOverrides {
            staff: Some(true),
            flags: overridden_flags,
        };

        let (staff, flags) = apply_feature_flags_overrides(
            false,
            vec!["server-only".to_string(), "diff-review".to_string()],
            Some(overrides),
        );

        assert!(staff);
        assert_eq!(
            flags,
            vec![
                "diff-review".to_string(),
                "local-only".to_string(),
                "split-diff".to_string(),
            ],
        );
    }
}
