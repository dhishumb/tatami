//! Running applications via NSWorkspace / NSRunningApplication.

use std::path::PathBuf;

use objc2_app_kit::{
    NSApplicationActivationOptions, NSApplicationActivationPolicy, NSRunningApplication,
    NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_foundation::NSString;
use tatami_core::{AppInfo, PlatformError};

pub(crate) fn info(app: &NSRunningApplication) -> AppInfo {
    AppInfo {
        pid: app.processIdentifier(),
        bundle_id: app.bundleIdentifier().map(|s| s.to_string()),
        name: app
            .localizedName()
            .map(|s| s.to_string())
            .unwrap_or_default(),
        exe: app
            .executableURL()
            .and_then(|u| u.path())
            .map(|p| PathBuf::from(p.to_string())),
    }
}

/// Apps with a Dock presence (activation policy `Regular`).
pub fn running_apps() -> Vec<AppInfo> {
    let mut apps: Vec<AppInfo> = NSWorkspace::sharedWorkspace()
        .runningApplications()
        .iter()
        .filter(|a| {
            a.activationPolicy() == NSApplicationActivationPolicy::Regular && !a.isTerminated()
        })
        .map(|a| info(&a))
        // A stale entry for a dead process reports pid -1.
        .filter(|a| a.pid > 0)
        .collect();
    // NSWorkspace is not the truth: an app that re-exec'd itself in place
    // (plyterm's hot swap) keeps a stale pid -1 record and its live pid is
    // listed nowhere. The window server knows who owns every on-screen
    // window, so anything with a normal-layer window is an app to manage.
    let me = std::process::id() as i32;
    for (pid, name) in window_owners() {
        if pid == me || apps.iter().any(|a| a.pid == pid) {
            continue;
        }
        let mut a = NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
            .filter(|a| a.activationPolicy() != NSApplicationActivationPolicy::Prohibited)
            .map(|a| info(&a))
            .unwrap_or(AppInfo {
                pid,
                bundle_id: None,
                name: name.clone(),
                exe: None,
            });
        // The record AppKit hands back for a re-exec'd app still says pid -1;
        // the window server's pid is the live one.
        a.pid = pid;
        if a.name.is_empty() {
            a.name = name;
        }
        apps.push(a);
    }
    apps
}

/// Owner pid and name of every on-screen window at the normal layer, from
/// the window server (no Accessibility needed). Deduplicated by pid.
pub fn window_owners() -> Vec<(i32, String)> {
    window_list()
        .into_iter()
        .fold(Vec::new(), |mut out, (pid, _, name)| {
            if !out.iter().any(|(p, _)| *p == pid) {
                out.push((pid, name));
            }
            out
        })
}

/// The frontmost normal window per the window server: (pid, window id).
pub fn front_window() -> Option<(i32, u32)> {
    window_list()
        .into_iter()
        .next()
        .map(|(pid, id, _)| (pid, id))
}

/// On-screen normal-layer windows front to back: (owner pid, window id, owner name).
fn window_list() -> Vec<(i32, u32, String)> {
    use objc2_core_foundation::{CFDictionary, CFNumber, CFNumberType, CFString};
    use objc2_core_graphics::{
        CGWindowListCopyWindowInfo, CGWindowListOption, kCGNullWindowID, kCGWindowLayer,
        kCGWindowNumber, kCGWindowOwnerName, kCGWindowOwnerPID,
    };
    let mut out: Vec<(i32, u32, String)> = Vec::new();
    let Some(list) = CGWindowListCopyWindowInfo(
        CGWindowListOption::OptionOnScreenOnly | CGWindowListOption::ExcludeDesktopElements,
        kCGNullWindowID,
    ) else {
        return out;
    };
    let num = |d: &CFDictionary, key: &CFString| -> Option<i64> {
        // SAFETY: the key is a valid CFString; a present value is a CFNumber.
        let v = unsafe { d.value(key as *const CFString as *const _) };
        if v.is_null() {
            return None;
        }
        let mut n: i64 = 0;
        // SAFETY: `v` is a live CFNumber owned by the dictionary; `n` is a valid i64 slot.
        let ok = unsafe {
            (*(v as *const CFNumber)).value(CFNumberType::SInt64Type, (&mut n as *mut i64).cast())
        };
        ok.then_some(n)
    };
    for i in 0..list.count() {
        // SAFETY: `i` is in range; each element of this list is a CFDictionary.
        let d = unsafe { &*(list.value_at_index(i) as *const CFDictionary) };
        // SAFETY: framework constants are valid for the process lifetime.
        let (layer_k, pid_k, name_k, id_k) = unsafe {
            (
                kCGWindowLayer,
                kCGWindowOwnerPID,
                kCGWindowOwnerName,
                kCGWindowNumber,
            )
        };
        if num(d, layer_k) != Some(0) {
            continue;
        }
        let Some(pid) = num(d, pid_k).map(|p| p as i32) else {
            continue;
        };
        let Some(id) = num(d, id_k).map(|p| p as u32) else {
            continue;
        };
        if pid <= 0 {
            continue;
        }
        // SAFETY: a present owner name is a CFString.
        let name_v = unsafe { d.value(name_k as *const CFString as *const _) };
        let name = if name_v.is_null() {
            String::new()
        } else {
            // SAFETY: non-null and a CFString per the key's contract.
            unsafe { (*(name_v as *const CFString)).to_string() }
        };
        out.push((pid, id, name));
    }
    out
}

/// The app with keyboard focus, from the Accessibility system-wide element.
/// This is the real answer; AppKit's `frontmostApplication` and the window
/// server's z-order are fallbacks (stale records, floating utility windows).
pub fn ax_focused_pid() -> Option<i32> {
    crate::ax::AxElement::system_wide()
        .attr_element("AXFocusedApplication")
        .ok()?
        .pid()
        .ok()
        .filter(|&p| p > 0)
}

/// The app whose own AX element says it is frontmost, scanning the apps
/// that own on-screen windows. Works for apps AppKit lost track of; costs
/// one AX read per app, so it is the last resort.
pub(crate) fn ax_frontmost_pid(shared: &crate::Shared) -> Option<i32> {
    window_owners()
        .into_iter()
        .map(|(pid, _)| pid)
        .find(|&pid| shared.app_el(pid).attr_bool("AXFrontmost").unwrap_or(false))
}

pub fn frontmost_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .map(|a| a.processIdentifier())
        .filter(|&p| p > 0)
}

/// The app that owns the menu bar is the active one. `frontmostApplication`
/// can be a stale entry with pid -1 (seen after an app was force-quit).
pub fn menu_bar_owner_pid() -> Option<i32> {
    NSWorkspace::sharedWorkspace()
        .menuBarOwningApplication()
        .map(|a| a.processIdentifier())
        .filter(|&p| p > 0)
}

/// Ask the app to become active. Returns whether the request was sent.
pub fn activate(pid: i32) -> bool {
    NSRunningApplication::runningApplicationWithProcessIdentifier(pid)
        .is_some_and(|a| a.activateWithOptions(NSApplicationActivationOptions::empty()))
}

/// Launch without activating; no-op if an instance is already running.
pub fn launch(bundle_id: &str) -> Result<(), PlatformError> {
    let id = NSString::from_str(bundle_id);
    if NSRunningApplication::runningApplicationsWithBundleIdentifier(&id).count() > 0 {
        return Ok(());
    }
    let ws = NSWorkspace::sharedWorkspace();
    let url = ws
        .URLForApplicationWithBundleIdentifier(&id)
        .ok_or_else(|| PlatformError::Other(format!("no app with bundle id {bundle_id}")))?;
    let cfg = NSWorkspaceOpenConfiguration::new();
    cfg.setActivates(false);
    // Fire and forget: the launch shows up later as an `AppLaunched` event.
    ws.openApplicationAtURL_configuration_completionHandler(&url, &cfg, None);
    Ok(())
}
