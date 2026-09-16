//! Early interaction profile discovery.
//!
//! Some games (Fallout 4 VR, Skyrim VR, Serious Sam 3 VR) read the HMD's identity properties
//! once, right after init, to decide on a control scheme - and never again. Our HMD identity is
//! derived from the controller interaction profile, but runtimes only resolve that on
//! xrSyncActions in a focused session, which normally isn't until the game submits its first
//! frame. So before the real session is created, we spin up a throwaway session, attach a
//! dummy action set bound to every supported profile, submit empty frames until the session is
//! focused, sync once and remember what got bound.

use super::profiles::{self, InteractionProfile, RunWithProfile};
use crate::openxr_data::{Hand, SessionData};
use log::{info, warn};
use openvr as vr;
use openxr as xr;
use std::time::{Duration, Instant};

/// The interaction profiles bound to each hand at init.
#[derive(Default)]
pub struct ProbedProfiles {
    pub left: Option<String>,
    pub right: Option<String>,
    /// The suggested bindings live on the instance and reference this action, and it's simplest
    /// to not have to worry about what runtimes do with bindings to destroyed actions, so keep
    /// it alive for as long as the instance.
    _probe_set: Option<(xr::ActionSet, xr::Action<xr::Posef>)>,
}

impl ProbedProfiles {
    pub fn get(&self, hand: Hand) -> Option<&str> {
        match hand {
            Hand::Left => self.left.as_deref(),
            Hand::Right => self.right.as_deref(),
        }
    }
}

const FOCUS_TIMEOUT: Duration = Duration::from_secs(1);

pub fn probe_interaction_profiles(
    instance: &xr::Instance,
    system_id: xr::SystemId,
    enabled_extensions: &xr::ExtensionSet,
) -> ProbedProfiles {
    let left = instance.string_to_path("/user/hand/left").unwrap();
    let right = instance.string_to_path("/user/hand/right").unwrap();

    // Every interaction profile supports the grip pose, so a single pose action is enough to get
    // all of them bound.
    let set = instance
        .create_action_set("xrizer_probe", "XRizer profile probe", 0)
        .unwrap();
    let action = set
        .create_action::<xr::Posef>("grip", "Grip", &[left, right])
        .unwrap();

    struct Suggester<'a> {
        instance: &'a xr::Instance,
        enabled_extensions: &'a xr::ExtensionSet,
        action: &'a xr::Action<xr::Posef>,
    }
    impl RunWithProfile for Suggester<'_> {
        fn run<P: InteractionProfile>(&mut self) {
            if !P::has_required_extensions(self.enabled_extensions) {
                return;
            }
            let profile = self.instance.string_to_path(P::profile_path()).unwrap();
            let bindings = [
                "/user/hand/left/input/grip/pose",
                "/user/hand/right/input/grip/pose",
            ]
            .map(|path| xr::Binding::new(self.action, self.instance.string_to_path(path).unwrap()));
            self.instance
                .suggest_interaction_profile_bindings(profile, &bindings)
                .unwrap();
        }
    }
    profiles::run_for_all_profiles(&mut Suggester {
        instance,
        enabled_extensions,
        action: &action,
    });

    let (session_data, mut waiter, stream) = match SessionData::new(
        instance,
        system_id,
        vr::ETrackingUniverseOrigin::Standing,
        None,
    ) {
        Ok(s) => s,
        Err(e) => {
            warn!(
                "Failed to create probe session, interaction profiles unknown until later: {e:?}"
            );
            return Default::default();
        }
    };
    // Temporary sessions are always created with Vulkan.
    let mut stream: xr::FrameStream<xr::Vulkan> =
        stream.try_into().unwrap_or_else(|_| unreachable!());
    let session = &session_data.session;

    session.attach_action_sets(&[&set]).unwrap();

    // Submit empty frames until the runtime focuses the session - profiles are only resolved on
    // xrSyncActions in a focused session.
    let mut state = xr::SessionState::READY;
    let mut buf = xr::EventDataBuffer::new();
    let mut poll = |state: &mut xr::SessionState| {
        while let Ok(Some(event)) = instance.poll_event(&mut buf) {
            if let xr::Event::SessionStateChanged(event) = event {
                *state = event.state();
            }
        }
    };

    let start = Instant::now();
    while state != xr::SessionState::FOCUSED && start.elapsed() < FOCUS_TIMEOUT {
        let submitted = waiter
            .wait()
            .and_then(|frame| stream.begin().map(|_| frame))
            .and_then(|frame| {
                stream.end(
                    frame.predicted_display_time,
                    xr::EnvironmentBlendMode::OPAQUE,
                    &[],
                )
            });
        if let Err(e) = submitted {
            warn!("Failed to submit empty frame to probe session: {e}");
            break;
        }
        poll(&mut state);
    }

    let mut probed = ProbedProfiles::default();
    if state == xr::SessionState::FOCUSED {
        if let Err(e) = session.sync_actions(&[xr::ActiveActionSet::new(&set)]) {
            warn!("Failed to sync probe actions: {e}");
        }
        poll(&mut state);
        let profile_for = |subaction| {
            session
                .current_interaction_profile(subaction)
                .ok()
                .filter(|p| *p != xr::Path::NULL)
                .map(|p| instance.path_to_string(p).unwrap())
        };
        probed.left = profile_for(left);
        probed.right = profile_for(right);
        info!(
            "Probed interaction profiles: left: {}, right: {}",
            probed.left.as_deref().unwrap_or("<null>"),
            probed.right.as_deref().unwrap_or("<null>")
        );
    } else {
        warn!("Probe session never became focused, interaction profiles unknown until later");
    }

    // Tear the session down cleanly before the real one gets created.
    if session.request_exit().is_ok() {
        while state != xr::SessionState::STOPPING && start.elapsed() < 2 * FOCUS_TIMEOUT {
            poll(&mut state);
        }
        let _ = session.end();
        while state != xr::SessionState::EXITING && start.elapsed() < 2 * FOCUS_TIMEOUT {
            poll(&mut state);
        }
    }
    drop(stream);
    drop(waiter);
    drop(session_data);

    probed._probe_set = Some((set, action));
    probed
}
