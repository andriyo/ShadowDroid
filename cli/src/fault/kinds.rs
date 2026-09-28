//! One function per fault kind. A state fault reads the state it replaces,
//! hands its restore plan to [`Recorder::record`] (which journals it) before
//! the first device change, applies the fault, and verifies it took effect.

use super::args::*;
use super::device::{self, quote};
use super::error;
use super::restore::{self, RestoreStep, Verify};
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::time::Duration;

pub struct Injected {
    pub params: Value,
    pub observed: Value,
    pub warnings: Vec<String>,
    pub next_actions: Vec<String>,
}

impl Injected {
    fn new(params: Value, observed: Value) -> Self {
        Self {
            params,
            observed,
            warnings: Vec::new(),
            next_actions: Vec::new(),
        }
    }
}

pub struct Ctx<'a> {
    pub serial: &'a str,
    pub id: &'a str,
    pub app: Option<&'a str>,
    pub recorder: &'a mut super::Recorder,
}

fn app<'a>(ctx: &Ctx<'a>) -> &'a str {
    ctx.app.expect("validated: kind needs an app")
}

fn shell_step(command: impl Into<String>, describe: impl Into<String>) -> RestoreStep {
    RestoreStep::Shell {
        command: command.into(),
        describe: describe.into(),
        verify: None,
    }
}

fn verified_step(
    command: impl Into<String>,
    describe: impl Into<String>,
    check: impl Into<String>,
    contains: impl Into<String>,
) -> RestoreStep {
    RestoreStep::Shell {
        command: command.into(),
        describe: describe.into(),
        verify: Some(Verify {
            command: check.into(),
            contains: contains.into(),
        }),
    }
}

fn setting_step(namespace: &str, key: &str, value: Option<String>) -> RestoreStep {
    RestoreStep::Setting {
        namespace: namespace.into(),
        key: key.into(),
        value,
    }
}

async fn verify(serial: &str, command: &str, contains: &str) -> Result<()> {
    restore::wait_for(
        serial,
        &Verify {
            command: command.into(),
            contains: contains.into(),
        },
    )
    .await
    .map_err(|error| error::verification_failed(format!("{error:#}")))
}

pub async fn inject(ctx: &mut Ctx<'_>, cmd: &InjectCmd) -> Result<Injected> {
    match cmd {
        InjectCmd::ProcessDeath {
            relaunch,
            timeout_ms,
            ..
        } => process_death(ctx, *relaunch, *timeout_ms).await,
        InjectCmd::DontKeepActivities { .. } => dont_keep_activities(ctx).await,
        InjectCmd::LowMemory { level, .. } => low_memory(ctx, *level).await,
        InjectCmd::RevokePermission { permission, .. } => revoke_permission(ctx, permission).await,
        InjectCmd::Orientation { rotation, .. } => orientation(ctx, *rotation).await,
        InjectCmd::NightMode { mode, .. } => night_mode(ctx, *mode).await,
        InjectCmd::FontScale { scale, .. } => font_scale(ctx, *scale).await,
        InjectCmd::DisplaySize { size, density, .. } => {
            display_size(ctx, size.as_deref(), *density).await
        }
        InjectCmd::AppLocale { locales, .. } => app_locale(ctx, locales).await,
        InjectCmd::SplitScreen { percent, .. } => split_screen(ctx, *percent).await,
        InjectCmd::Pip { .. } => pip(ctx).await,
        InjectCmd::Doze { .. } => doze(ctx).await,
        InjectCmd::StandbyBucket { bucket, .. } => standby_bucket(ctx, *bucket).await,
        InjectCmd::Battery { level, saver, .. } => battery(ctx, *level, *saver).await,
        InjectCmd::Thermal { status, .. } => thermal(ctx, *status).await,
        InjectCmd::StorageFull { free_mb, .. } => storage_full(ctx, *free_mb).await,
        InjectCmd::Clock { offset_ms, .. } => clock(ctx, *offset_ms).await,
        InjectCmd::Timezone { tz, .. } => timezone(ctx, tz).await,
        InjectCmd::CpuLoad { threads, .. } => cpu_load(ctx, *threads).await,
        InjectCmd::AirplaneMode { .. } => airplane_mode(ctx).await,
        InjectCmd::WifiOff { .. } => {
            radio_off(
                ctx,
                "wifi_on",
                "svc wifi disable",
                "svc wifi enable",
                "Wi-Fi",
            )
            .await
        }
        InjectCmd::MobileDataOff { .. } => {
            radio_off(
                ctx,
                "mobile_data",
                "svc data disable",
                "svc data enable",
                "mobile data",
            )
            .await
        }
        InjectCmd::NetworkFlap { period_ms, .. } => network_flap(ctx, *period_ms).await,
        InjectCmd::DnsFailure { .. } => dns_failure(ctx).await,
        InjectCmd::NetworkSpeed { profile, .. } => network_speed(ctx, *profile).await,
        InjectCmd::NetworkLatency { profile, .. } => network_latency(ctx, *profile).await,
        InjectCmd::HttpErrors { .. }
        | InjectCmd::HttpLatency { .. }
        | InjectCmd::Bandwidth { .. }
        | InjectCmd::ConnectionReset { .. }
        | InjectCmd::TruncatedResponse { .. }
        | InjectCmd::TlsFailure { .. } => super::proxy::inject(ctx, cmd).await,
        InjectCmd::IncomingCall { number, .. } => incoming_call(ctx, number).await,
        InjectCmd::Sms { from, text } => sms(ctx, from, text).await,
        InjectCmd::ScreenOff { .. } => screen_off(ctx).await,
        InjectCmd::NotificationShade { panel, .. } => notification_shade(ctx, *panel).await,
        InjectCmd::EmulatorCrash {
            relaunch,
            cold_boot,
        } => emulator_crash(ctx, *relaunch, *cold_boot).await,
    }
}

// ── app lifecycle ─────────────────────────────────────────────────────────

async fn running_pid(ctx: &Ctx<'_>) -> Result<u32> {
    let package = app(ctx);
    device::pid_of(ctx.serial, package)
        .await?
        .ok_or_else(|| error::app_not_running(ctx.serial, package))
}

async fn process_death(ctx: &mut Ctx<'_>, relaunch: bool, timeout_ms: u32) -> Result<Injected> {
    let package = app(ctx);
    let pid = running_pid(ctx).await?;
    // Reopening must return to this task, as a user switching back does.
    let root_activity = task_root_activity(ctx.serial, package).await?;
    device::run_ok(ctx.serial, "input keyevent KEYCODE_HOME").await?;
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_millis(u64::from(timeout_ms));
    let mut killed_by = None;
    // `am kill` only kills a process Android already considers cached, which
    // takes a few seconds after the app leaves the foreground.
    while std::time::Instant::now() < deadline {
        device::run(ctx.serial, &format!("am kill {}", quote(package))).await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        if device::pid_of(ctx.serial, package).await? != Some(pid) {
            killed_by = Some("am_kill");
            break;
        }
    }
    if killed_by.is_none() {
        // A debuggable app can be killed as its own user, which also keeps
        // its task and saved state.
        let ran = device::run(
            ctx.serial,
            &format!("run-as {} kill -9 {pid}", quote(package)),
        )
        .await?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        if ran.ok() && device::pid_of(ctx.serial, package).await? != Some(pid) {
            killed_by = Some("run_as_kill");
        }
    }
    let Some(killed_by) = killed_by else {
        return Err(error::verification_failed(format!(
            "Android kept {package} (pid {pid}) alive for {timeout_ms} ms after it left the foreground (a foreground service or bound process keeps it); it is not debuggable, so it could not be killed directly"
        )));
    };
    let mut observed = json!({
        "pid_before": pid,
        "killed_by": killed_by,
        "waited_ms": started.elapsed().as_millis() as u64,
        "task_kept": true,
    });
    let mut result = Injected::new(
        json!({"relaunch": relaunch, "timeout_ms": timeout_ms}),
        json!({}),
    );
    if relaunch {
        let activity = match root_activity {
            Some(activity) => activity,
            None => launcher_activity(ctx.serial, package).await?,
        };
        device::run_ok(
            ctx.serial,
            &format!(
                "am start -a android.intent.action.MAIN -c android.intent.category.LAUNCHER -n {}",
                quote(&activity)
            ),
        )
        .await?;
        let mut new_pid = None;
        for _ in 0..20 {
            tokio::time::sleep(Duration::from_millis(250)).await;
            new_pid = device::pid_of(ctx.serial, package).await?;
            if new_pid.is_some() {
                break;
            }
        }
        observed["relaunched_pid"] = json!(new_pid);
        observed["relaunched_activity"] = json!(activity);
    } else {
        result
            .next_actions
            .push(format!("shadowdroid -d {} app start {package}", ctx.serial));
    }
    result.observed = observed;
    Ok(result)
}

/// The root activity of the app's most recent task, from recents.
async fn task_root_activity(serial: &str, package: &str) -> Result<Option<String>> {
    let text = device::read_ok(serial, "dumpsys activity recents").await?;
    let needle = format!("realActivity={{{package}/");
    Ok(text.split("* Recent #").find_map(|task| {
        let start = task.find(&needle)? + "realActivity={".len();
        let end = task[start..].find('}')?;
        Some(task[start..start + end].to_string())
    }))
}

async fn launcher_activity(serial: &str, package: &str) -> Result<String> {
    let text = device::read_ok(
        serial,
        &format!(
            "cmd package resolve-activity --brief -a android.intent.action.MAIN -c android.intent.category.LAUNCHER {}",
            quote(package)
        ),
    )
    .await?;
    let activity = text
        .lines()
        .rev()
        .map(str::trim)
        .find(|line| line.contains('/'))
        .ok_or_else(|| anyhow::anyhow!("{package} has no launcher activity"))?;
    if !activity.starts_with(&format!("{package}/")) {
        // Several launcher activities resolve to Android's chooser.
        return Err(error::invalid_param(format!(
            "{package} has more than one launcher activity and no task to return to; start it with `app start` instead of --relaunch"
        )));
    }
    Ok(activity.to_string())
}

async fn dont_keep_activities(ctx: &mut Ctx<'_>) -> Result<Injected> {
    let prior = device::get_setting(ctx.serial, "global", "always_finish_activities").await?;
    if prior.as_deref() == Some("1") {
        return Err(error::no_effect("Don't keep activities is already on"));
    }
    let params = json!({});
    ctx.recorder
        .record(
            &params,
            vec![setting_step("global", "always_finish_activities", prior)],
        )
        .await?;
    device::put_setting(ctx.serial, "global", "always_finish_activities", Some("1")).await?;
    Ok(Injected::new(
        params,
        json!({"always_finish_activities": 1}),
    ))
}

async fn low_memory(ctx: &mut Ctx<'_>, level: TrimLevel) -> Result<Injected> {
    let package = app(ctx);
    let pid = running_pid(ctx).await?;
    let ran = device::run(
        ctx.serial,
        &format!(
            "am send-trim-memory {} {}",
            quote(package),
            level.android_name()
        ),
    )
    .await?;
    if !ran.ok() || ran.output.contains("Exception") {
        let hint = if level.needs_foreground() {
            "running-* levels reach only a foreground app: bring it to the front, or pick ui-hidden/background/moderate/complete for a backgrounded app"
        } else {
            "background levels reach only a backgrounded app: press HOME first, or pick a running-* level for a foreground app"
        };
        return Err(error::device_rejected(
            &ran.output,
            json!({"level": level.name(), "hint": hint}),
        ));
    }
    Ok(Injected::new(
        json!({"level": level.name()}),
        json!({"pid": pid, "android_level": level.android_name()}),
    ))
}

async fn permission_granted(serial: &str, package: &str, permission: &str) -> Result<Option<bool>> {
    let text = device::read(
        serial,
        &format!(
            "dumpsys package {} | grep -F {}",
            quote(package),
            quote(&format!("{permission}: granted="))
        ),
    )
    .await?
    .output;
    Ok(if text.contains("granted=true") {
        Some(true)
    } else if text.contains("granted=false") {
        Some(false)
    } else {
        None
    })
}

async fn revoke_permission(ctx: &mut Ctx<'_>, permission: &str) -> Result<Injected> {
    let package = app(ctx);
    match permission_granted(ctx.serial, package, permission).await? {
        Some(true) => {}
        Some(false) => {
            return Err(error::no_effect(format!(
                "{permission} is not granted to {package}, so revoking it changes nothing"
            )));
        }
        None => {
            return Err(error::invalid_param(format!(
                "{package} does not request the runtime permission {permission}"
            )));
        }
    }
    let params = json!({"permission": permission});
    let check = format!(
        "dumpsys package {} | grep -F {}",
        quote(package),
        quote(&format!("{permission}: granted="))
    );
    ctx.recorder
        .record(
            &params,
            vec![verified_step(
                format!("pm grant {} {}", quote(package), quote(permission)),
                format!("grant {permission} to {package} again"),
                check.clone(),
                "granted=true",
            )],
        )
        .await?;
    device::run_ok(
        ctx.serial,
        &format!("pm revoke {} {}", quote(package), quote(permission)),
    )
    .await?;
    verify(ctx.serial, &check, "granted=false").await?;
    Ok(Injected::new(
        params,
        json!({"was_granted": true, "granted": false, "app_killed_by_android": true}),
    ))
}

// ── configuration ─────────────────────────────────────────────────────────

async fn orientation(ctx: &mut Ctx<'_>, rotation: Rotation) -> Result<Injected> {
    let auto = device::get_setting(ctx.serial, "system", "accelerometer_rotation").await?;
    let user = device::get_setting(ctx.serial, "system", "user_rotation").await?;
    let wanted = rotation.index().to_string();
    if auto.as_deref() == Some("0") && user.as_deref() == Some(wanted.as_str()) {
        return Err(error::no_effect(format!(
            "the screen is already locked at {} degrees",
            rotation.degrees()
        )));
    }
    let params = json!({"rotation": rotation.degrees()});
    ctx.recorder
        .record(
            &params,
            vec![
                setting_step("system", "user_rotation", user),
                setting_step("system", "accelerometer_rotation", auto),
            ],
        )
        .await?;
    device::put_setting(ctx.serial, "system", "accelerometer_rotation", Some("0")).await?;
    device::put_setting(ctx.serial, "system", "user_rotation", Some(&wanted)).await?;
    Ok(Injected::new(
        params,
        json!({"rotation_degrees": rotation.degrees(), "auto_rotate": false}),
    ))
}

async fn night_mode(ctx: &mut Ctx<'_>, mode: OnOff) -> Result<Injected> {
    let current = device::read_ok(ctx.serial, "cmd uimode night").await?;
    // "Night mode: no" / "yes" / "auto" / "custom"
    let prior = current
        .rsplit(':')
        .next()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("no")
        .to_string();
    let wanted = match mode {
        OnOff::On => "yes",
        OnOff::Off => "no",
    };
    if prior == wanted {
        return Err(error::no_effect(format!("night mode is already {wanted}")));
    }
    let params = json!({"mode": mode.name()});
    ctx.recorder
        .record(
            &params,
            vec![verified_step(
                format!("cmd uimode night {}", quote(&prior)),
                format!("set night mode back to {prior}"),
                "cmd uimode night",
                format!("Night mode: {prior}"),
            )],
        )
        .await?;
    device::run_ok(ctx.serial, &format!("cmd uimode night {wanted}")).await?;
    verify(
        ctx.serial,
        "cmd uimode night",
        &format!("Night mode: {wanted}"),
    )
    .await?;
    Ok(Injected::new(
        params,
        json!({"night_mode": wanted, "was": prior}),
    ))
}

async fn font_scale(ctx: &mut Ctx<'_>, scale: f32) -> Result<Injected> {
    if !(0.5..=3.0).contains(&scale) {
        return Err(error::invalid_param(format!(
            "--scale {scale} is outside 0.5-3.0"
        )));
    }
    let prior = device::get_setting(ctx.serial, "system", "font_scale").await?;
    let prior_value: f32 = prior.as_deref().and_then(|v| v.parse().ok()).unwrap_or(1.0);
    if (prior_value - scale).abs() < 0.001 {
        return Err(error::no_effect(format!(
            "the font scale is already {scale}"
        )));
    }
    let params = json!({"scale": scale});
    ctx.recorder
        .record(&params, vec![setting_step("system", "font_scale", prior)])
        .await?;
    device::put_setting(ctx.serial, "system", "font_scale", Some(&scale.to_string())).await?;
    Ok(Injected::new(
        params,
        json!({"font_scale": scale, "was": prior_value}),
    ))
}

/// `wm size` / `wm density` output: (physical, override).
fn parse_wm(output: &str) -> (Option<String>, Option<String>) {
    let field = |label: &str| {
        output
            .lines()
            .find(|line| line.trim_start().starts_with(label))
            .and_then(|line| line.split(':').nth(1))
            .map(|value| value.trim().to_string())
    };
    (field("Physical"), field("Override"))
}

async fn display_size(
    ctx: &mut Ctx<'_>,
    size: Option<&str>,
    density: Option<u32>,
) -> Result<Injected> {
    if let Some(size) = size {
        let valid = size.split_once('x').is_some_and(|(w, h)| {
            w.parse::<u32>().is_ok_and(|w| w >= 200) && h.parse::<u32>().is_ok_and(|h| h >= 200)
        });
        if !valid {
            return Err(error::invalid_param(format!(
                "--size {size} is not WIDTHxHEIGHT with both at least 200"
            )));
        }
    }
    if density.is_some_and(|d| !(72..=1_000).contains(&d)) {
        return Err(error::invalid_param("--density must be 72-1000 dpi"));
    }
    let (_, size_override) = parse_wm(&device::read_ok(ctx.serial, "wm size").await?);
    let (_, density_override) = parse_wm(&device::read_ok(ctx.serial, "wm density").await?);
    let params = json!({"size": size, "density": density});
    let mut steps = Vec::new();
    if size.is_some() {
        let back = size_override.clone().unwrap_or_else(|| "reset".into());
        steps.push(shell_step(
            format!("wm size {}", quote(&back)),
            format!("set the display size back to {back}"),
        ));
    }
    if density.is_some() {
        let back = density_override.clone().unwrap_or_else(|| "reset".into());
        steps.push(shell_step(
            format!("wm density {}", quote(&back)),
            format!("set the display density back to {back}"),
        ));
    }
    ctx.recorder.record(&params, steps).await?;
    if let Some(size) = size {
        device::run_ok(ctx.serial, &format!("wm size {}", quote(size))).await?;
        verify(ctx.serial, "wm size", &format!("Override size: {size}")).await?;
    }
    if let Some(density) = density {
        device::run_ok(ctx.serial, &format!("wm density {density}")).await?;
        verify(
            ctx.serial,
            "wm density",
            &format!("Override density: {density}"),
        )
        .await?;
    }
    Ok(Injected::new(
        params,
        json!({"size": size, "density": density, "was": {"size_override": size_override, "density_override": density_override}}),
    ))
}

fn parse_app_locales(output: &str) -> String {
    output
        .rsplit_once('[')
        .and_then(|(_, rest)| rest.split_once(']'))
        .map(|(inside, _)| inside.replace(' ', ""))
        .unwrap_or_default()
}

async fn app_locale(ctx: &mut Ctx<'_>, locales: &str) -> Result<Injected> {
    let package = app(ctx);
    let wanted = locales.replace(' ', "");
    if wanted.is_empty()
        || !wanted.split(',').all(|tag| {
            !tag.is_empty() && tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
    {
        return Err(error::invalid_param(format!(
            "--locales {locales:?} is not a comma-separated list of language tags"
        )));
    }
    let get = format!("cmd locale get-app-locales {}", quote(package));
    let prior = parse_app_locales(&device::read_ok(ctx.serial, &get).await?);
    if prior == wanted {
        return Err(error::no_effect(format!("{package} already uses {wanted}")));
    }
    let params = json!({"locales": wanted});
    ctx.recorder
        .record(
            &params,
            vec![verified_step(
                format!(
                    "cmd locale set-app-locales {} --locales {}",
                    quote(package),
                    quote(&prior)
                ),
                if prior.is_empty() {
                    format!("return {package} to the system language")
                } else {
                    format!("set {package}'s locales back to {prior}")
                },
                get.clone(),
                format!("[{}]", prior.replace(',', ", ")),
            )],
        )
        .await?;
    device::run_ok(
        ctx.serial,
        &format!(
            "cmd locale set-app-locales {} --locales {}",
            quote(package),
            quote(&wanted)
        ),
    )
    .await?;
    verify(
        ctx.serial,
        &get,
        &format!("[{}]", wanted.replace(',', ", ")),
    )
    .await?;
    Ok(Injected::new(
        params,
        json!({"locales": wanted, "was": prior}),
    ))
}

/// The app's task id, top activity and bounds from `am stack list`.
async fn app_task(serial: &str, package: &str) -> Result<Option<(u32, String, String)>> {
    let text = device::read_ok(serial, "am stack list").await?;
    let needle = format!(": {package}/");
    Ok(text
        .lines()
        .find(|line| line.contains(&needle))
        .and_then(|line| {
            let line = line.trim();
            let id = line
                .strip_prefix("taskId=")?
                .split(':')
                .next()?
                .parse()
                .ok()?;
            let component = line
                .split(": ")
                .nth(1)?
                .split_whitespace()
                .next()?
                .to_string();
            let bounds = line
                .split("bounds=")
                .nth(1)?
                .split_whitespace()
                .next()?
                .to_string();
            Some((id, component, bounds))
        }))
}

async fn display_pixels(serial: &str) -> Result<(u32, u32)> {
    let (physical, size_override) = parse_wm(&device::read_ok(serial, "wm size").await?);
    let size = size_override
        .or(physical)
        .ok_or_else(|| anyhow::anyhow!("unreadable `wm size`"))?;
    let (w, h) = size
        .split_once('x')
        .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
        .ok_or_else(|| anyhow::anyhow!("unreadable display size {size}"))?;
    Ok((w, h))
}

async fn split_screen(ctx: &mut Ctx<'_>, percent: u8) -> Result<Injected> {
    let package = app(ctx);
    let Some((task, component, _)) = app_task(ctx.serial, package).await? else {
        return Err(error::app_not_running(ctx.serial, package));
    };
    let (width, height) = display_pixels(ctx.serial).await?;
    let shrunk = height * u32::from(percent) / 100;
    let params = json!({"percent": percent});
    ctx.recorder
        .record(
            &params,
            vec![
                shell_step(
                    format!("am task resize {task} 0 0 {width} {height}"),
                    format!("resize task {task} back to the full screen"),
                ),
                shell_step(
                    format!("am start --windowingMode 1 -n {}", quote(&component)),
                    format!("return task {task} to fullscreen mode"),
                ),
            ],
        )
        .await?;
    device::run_ok(
        ctx.serial,
        &format!("am start --windowingMode 6 -n {}", quote(&component)),
    )
    .await?;
    device::run_ok(
        ctx.serial,
        &format!("am task resize {task} 0 0 {width} {shrunk}"),
    )
    .await?;
    let expected = format!("bounds=[0,0][{width},{shrunk}]");
    verify(
        ctx.serial,
        &format!("am stack list | grep -F 'taskId={task}:'"),
        &expected,
    )
    .await?;
    Ok(Injected::new(
        params,
        json!({"task_id": task, "bounds": [0, 0, width, shrunk], "display": [width, height]}),
    ))
}

async fn pip(ctx: &mut Ctx<'_>) -> Result<Injected> {
    let package = app(ctx);
    let pid = running_pid(ctx).await?;
    device::run_ok(ctx.serial, "input keyevent KEYCODE_HOME").await?;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    let pinned = device::read(
        ctx.serial,
        "dumpsys activity activities | grep -E 'mWindowingMode=pinned|windowingMode=pinned|realActivity'",
    )
    .await?
    .output;
    // A pinned root task is listed with the app's activity right after it.
    let entered = pinned
        .lines()
        .skip_while(|line| !line.contains("pinned"))
        .take(4)
        .any(|line| line.contains(package));
    let mut result = Injected::new(json!({}), json!({"pid": pid, "entered_pip": entered}));
    if !entered {
        result.warnings.push(format!(
            "{package} left the foreground but did not enter picture-in-picture (it may not support auto-enter on leave)"
        ));
    }
    Ok(result)
}

// ── power ─────────────────────────────────────────────────────────────────

async fn doze(ctx: &mut Ctx<'_>) -> Result<Injected> {
    let deep = device::read_ok(ctx.serial, "dumpsys deviceidle get deep").await?;
    if deep.trim() == "IDLE" {
        return Err(error::no_effect("the device is already in deep Doze"));
    }
    let params = json!({});
    ctx.recorder
        .record(
            &params,
            vec![shell_step(
                "dumpsys deviceidle unforce",
                "leave forced Doze",
            )],
        )
        .await?;
    device::run_ok(ctx.serial, "dumpsys deviceidle force-idle").await?;
    verify(ctx.serial, "dumpsys deviceidle get deep", "IDLE").await?;
    Ok(Injected::new(
        params,
        json!({"deep_idle": "IDLE", "was": deep.trim()}),
    ))
}

fn bucket_name(value: &str) -> &str {
    match value.trim() {
        "5" => "exempted",
        "10" => "active",
        "20" => "working_set",
        "30" => "frequent",
        "40" => "rare",
        "45" => "restricted",
        "50" => "never",
        other => other,
    }
}

async fn standby_bucket(ctx: &mut Ctx<'_>, bucket: Bucket) -> Result<Injected> {
    let package = app(ctx);
    let get = format!("am get-standby-bucket {}", quote(package));
    let prior = device::read_ok(ctx.serial, &get).await?.trim().to_string();
    let wanted = bucket.android_value().to_string();
    if prior == wanted {
        return Err(error::no_effect(format!(
            "{package} is already in the {} bucket",
            bucket.name()
        )));
    }
    if matches!(bucket_name(&prior), "exempted" | "never") {
        return Err(error::invalid_param(format!(
            "{package} is in the {} bucket, which Android does not let a shell change",
            bucket_name(&prior)
        )));
    }
    let params = json!({"bucket": bucket.name()});
    ctx.recorder
        .record(
            &params,
            vec![verified_step(
                format!(
                    "am set-standby-bucket {} {}",
                    quote(package),
                    bucket_name(&prior)
                ),
                format!("move {package} back to the {} bucket", bucket_name(&prior)),
                get.clone(),
                prior.clone(),
            )],
        )
        .await?;
    device::run_ok(
        ctx.serial,
        &format!(
            "am set-standby-bucket {} {}",
            quote(package),
            bucket_name(&wanted)
        ),
    )
    .await?;
    verify(ctx.serial, &get, &wanted).await?;
    Ok(Injected::new(
        params,
        json!({"bucket": bucket.name(), "was": bucket_name(&prior)}),
    ))
}

async fn battery(ctx: &mut Ctx<'_>, level: Option<u8>, saver: bool) -> Result<Injected> {
    let prior_saver = device::get_setting(ctx.serial, "global", "low_power").await?;
    let params = json!({"level": level, "saver": saver});
    let mut steps = vec![shell_step(
        "dumpsys battery reset",
        "stop overriding the battery reading",
    )];
    if saver {
        steps.push(setting_step("global", "low_power", prior_saver.clone()));
    }
    ctx.recorder.record(&params, steps).await?;
    device::run_ok(ctx.serial, "dumpsys battery unplug").await?;
    if let Some(level) = level {
        device::run_ok(ctx.serial, &format!("dumpsys battery set level {level}")).await?;
        verify(ctx.serial, "dumpsys battery", &format!("level: {level}")).await?;
    }
    verify(ctx.serial, "dumpsys battery", "AC powered: false").await?;
    if saver {
        // Battery Saver only stays on while the battery reports unplugged.
        device::put_setting(ctx.serial, "global", "low_power", Some("1")).await?;
    }
    Ok(Injected::new(
        params,
        json!({"unplugged": true, "level": level, "battery_saver": saver}),
    ))
}

async fn thermal(ctx: &mut Ctx<'_>, status: ThermalStatus) -> Result<Injected> {
    let params = json!({"status": status.name()});
    ctx.recorder
        .record(
            &params,
            vec![verified_step(
                "cmd thermalservice reset",
                "unlock the thermal status",
                "dumpsys thermalservice",
                "IsStatusOverride: false",
            )],
        )
        .await?;
    device::run_ok(
        ctx.serial,
        &format!("cmd thermalservice override-status {}", status.code()),
    )
    .await?;
    verify(
        ctx.serial,
        "dumpsys thermalservice",
        &format!("Thermal Status: {}", status.code()),
    )
    .await?;
    Ok(Injected::new(
        params,
        json!({"thermal_status": status.name(), "code": status.code()}),
    ))
}

// ── resources ─────────────────────────────────────────────────────────────

/// Available KiB on the data partition.
async fn data_free_kb(serial: &str) -> Result<u64> {
    let text = device::read_ok(serial, "df -k /data | tail -1").await?;
    text.split_whitespace()
        .nth(3)
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| anyhow::anyhow!("unreadable `df -k /data`: {text}"))
}

async fn storage_full(ctx: &mut Ctx<'_>, free_mb: u32) -> Result<Injected> {
    let free_kb = data_free_kb(ctx.serial).await?;
    let keep_kb = u64::from(free_mb) * 1024;
    if free_kb <= keep_kb {
        return Err(error::no_effect(format!(
            "only {} MiB is free already",
            free_kb / 1024
        )));
    }
    let fill_kb = free_kb - keep_kb;
    let path = format!("/data/local/tmp/.shadowdroid-fault-fill-{}", ctx.id);
    let params = json!({"free_mb": free_mb});
    ctx.recorder
        .record(
            &params,
            vec![RestoreStep::RemoveFile { path: path.clone() }],
        )
        .await?;
    device::run_ok(
        ctx.serial,
        &format!("fallocate -l {fill_kb}K {}", quote(&path)),
    )
    .await?;
    let after_kb = data_free_kb(ctx.serial).await?;
    if after_kb > keep_kb + keep_kb / 2 + 8 * 1024 {
        return Err(error::verification_failed(format!(
            "{} MiB is still free after filling",
            after_kb / 1024
        )));
    }
    Ok(Injected::new(
        params,
        json!({"free_mb_before": free_kb / 1024, "free_mb_after": after_kb / 1024, "filler": path}),
    ))
}

async fn clock(ctx: &mut Ctx<'_>, offset_ms: i64) -> Result<Injected> {
    if offset_ms == 0 {
        return Err(error::no_effect("--offset-ms 0 leaves the clock unchanged"));
    }
    let epoch_ms = restore::device_epoch_ms(ctx.serial).await?;
    let uptime_ms = restore::uptime_ms_now(ctx.serial).await?;
    let target = i64::try_from(epoch_ms)
        .ok()
        .and_then(|now| now.checked_add(offset_ms))
        .filter(|target| *target > 0)
        .ok_or_else(|| error::invalid_param("--offset-ms moves the clock before 1970"))?;
    let prior_auto = device::get_setting(ctx.serial, "global", "auto_time").await?;
    let params = json!({"offset_ms": offset_ms});
    ctx.recorder
        .record(
            &params,
            vec![
                RestoreStep::Clock {
                    epoch_ms,
                    uptime_ms,
                },
                setting_step("global", "auto_time", prior_auto),
            ],
        )
        .await?;
    device::put_setting(ctx.serial, "global", "auto_time", Some("0")).await?;
    device::run_ok(ctx.serial, &format!("cmd alarm set-time {target}")).await?;
    let now = restore::device_epoch_ms(ctx.serial).await?;
    if now.abs_diff(target as u64) > 10_000 {
        return Err(error::verification_failed(format!(
            "the device clock reads {now} ms after being set to {target} ms (it may not allow shell time changes)"
        )));
    }
    Ok(Injected::new(
        params,
        json!({"device_time_ms": now, "was_ms": epoch_ms, "automatic_time": false}),
    ))
}

async fn timezone(ctx: &mut Ctx<'_>, tz: &str) -> Result<Injected> {
    if tz.is_empty()
        || !tz
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/_+-".contains(c))
    {
        return Err(error::invalid_param(format!(
            "--tz {tz:?} is not an Olson time zone id"
        )));
    }
    let prior = device::read_ok(ctx.serial, "getprop persist.sys.timezone")
        .await?
        .trim()
        .to_string();
    if prior == tz {
        return Err(error::no_effect(format!("the time zone is already {tz}")));
    }
    let prior_auto = device::get_setting(ctx.serial, "global", "auto_time_zone").await?;
    let params = json!({"tz": tz});
    let mut steps = Vec::new();
    if !prior.is_empty() {
        steps.push(verified_step(
            format!("cmd alarm set-timezone {}", quote(&prior)),
            format!("set the time zone back to {prior}"),
            "getprop persist.sys.timezone",
            prior.clone(),
        ));
    }
    steps.push(setting_step("global", "auto_time_zone", prior_auto));
    ctx.recorder.record(&params, steps).await?;
    device::put_setting(ctx.serial, "global", "auto_time_zone", Some("0")).await?;
    device::run_ok(ctx.serial, &format!("cmd alarm set-timezone {}", quote(tz))).await?;
    verify(ctx.serial, "getprop persist.sys.timezone", tz).await?;
    Ok(Injected::new(params, json!({"timezone": tz, "was": prior})))
}

async fn cpu_load(ctx: &mut Ctx<'_>, threads: Option<u8>) -> Result<Injected> {
    let cores: u8 = device::read_ok(ctx.serial, "grep -c ^processor /proc/cpuinfo")
        .await?
        .trim()
        .parse()
        .unwrap_or(1);
    let threads = threads.unwrap_or(cores.max(1));
    let dir = format!("/data/local/tmp/.shadowdroid-fault-cpu-{}", ctx.id);
    let params = json!({"threads": threads});
    ctx.recorder
        .record(&params, vec![RestoreStep::KillPids { dir: dir.clone() }])
        .await?;
    let qdir = quote(&dir);
    device::run_ok(
        ctx.serial,
        &format!(
            "mkdir -p {qdir}; i=1; while [ $i -le {threads} ]; do nohup sh -c \"echo \\$\\$ > {dir}/$i.pid; while :; do :; done\" >/dev/null 2>&1 & i=$((i+1)); done"
        ),
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;
    let alive = device::read_ok(
        ctx.serial,
        &format!(
            "n=0; for f in {qdir}/*.pid; do kill -0 $(cat \"$f\") 2>/dev/null && n=$((n+1)); done; echo $n"
        ),
    )
    .await?;
    if alive.trim() != threads.to_string() {
        return Err(error::verification_failed(format!(
            "{} of {threads} busy loops are running",
            alive.trim()
        )));
    }
    Ok(Injected::new(
        params,
        json!({"threads": threads, "cores": cores}),
    ))
}

// ── network ───────────────────────────────────────────────────────────────

async fn airplane_mode(ctx: &mut Ctx<'_>) -> Result<Injected> {
    let prior = device::get_setting(ctx.serial, "global", "airplane_mode_on").await?;
    if prior.as_deref() == Some("1") {
        return Err(error::no_effect("airplane mode is already on"));
    }
    let params = json!({});
    ctx.recorder
        .record(
            &params,
            vec![verified_step(
                "cmd connectivity airplane-mode disable",
                "turn airplane mode off",
                "settings get global airplane_mode_on",
                "0",
            )],
        )
        .await?;
    device::run_ok(ctx.serial, "cmd connectivity airplane-mode enable").await?;
    verify(ctx.serial, "settings get global airplane_mode_on", "1").await?;
    Ok(Injected::new(params, json!({"airplane_mode": true})))
}

async fn radio_off(
    ctx: &mut Ctx<'_>,
    setting: &str,
    disable: &str,
    enable: &str,
    label: &str,
) -> Result<Injected> {
    let prior = device::get_setting(ctx.serial, "global", setting).await?;
    if prior.as_deref() == Some("0") {
        return Err(error::no_effect(format!("{label} is already off")));
    }
    let check = format!("settings get global {setting}");
    let params = json!({});
    ctx.recorder
        .record(
            &params,
            vec![verified_step(
                enable,
                format!("turn {label} back on"),
                check.clone(),
                "1",
            )],
        )
        .await?;
    device::run_ok(ctx.serial, disable).await?;
    verify(ctx.serial, &check, "0").await?;
    Ok(Injected::new(params, json!({setting: 0})))
}

async fn network_flap(ctx: &mut Ctx<'_>, period_ms: u32) -> Result<Injected> {
    let prior = device::get_setting(ctx.serial, "global", "airplane_mode_on").await?;
    let back = if prior.as_deref() == Some("1") {
        "enable"
    } else {
        "disable"
    };
    let dir = format!("/data/local/tmp/.shadowdroid-fault-flap-{}", ctx.id);
    let seconds = format!("{}.{:03}", period_ms / 1000, period_ms % 1000);
    let params = json!({"period_ms": period_ms});
    ctx.recorder
        .record(
            &params,
            vec![
                RestoreStep::KillPids { dir: dir.clone() },
                shell_step(
                    format!("cmd connectivity airplane-mode {back}"),
                    format!(
                        "leave airplane mode {}",
                        if back == "enable" { "on" } else { "off" }
                    ),
                ),
            ],
        )
        .await?;
    device::run_ok(
        ctx.serial,
        &format!(
            "mkdir -p {q}; nohup sh -c \"echo \\$\\$ > {dir}/flap.pid; while :; do cmd connectivity airplane-mode enable; sleep {seconds}; cmd connectivity airplane-mode disable; sleep {seconds}; done\" >/dev/null 2>&1 &",
            q = quote(&dir)
        ),
    )
    .await?;
    verify(ctx.serial, "settings get global airplane_mode_on", "1").await?;
    Ok(Injected::new(
        params,
        json!({"period_ms": period_ms, "cycle": "airplane mode on, then off, repeating"}),
    ))
}

const DNS_FAILURE_HOST: &str = "dns-failure.shadowdroid.invalid";

async fn dns_failure(ctx: &mut Ctx<'_>) -> Result<Injected> {
    let mode = device::get_setting(ctx.serial, "global", "private_dns_mode").await?;
    let host = device::get_setting(ctx.serial, "global", "private_dns_specifier").await?;
    if mode.as_deref() == Some("hostname") && host.as_deref() == Some(DNS_FAILURE_HOST) {
        return Err(error::no_effect("DNS is already failing"));
    }
    let params = json!({});
    ctx.recorder
        .record(
            &params,
            vec![
                setting_step("global", "private_dns_mode", mode.clone()),
                setting_step("global", "private_dns_specifier", host.clone()),
            ],
        )
        .await?;
    device::put_setting(
        ctx.serial,
        "global",
        "private_dns_specifier",
        Some(DNS_FAILURE_HOST),
    )
    .await?;
    device::put_setting(ctx.serial, "global", "private_dns_mode", Some("hostname")).await?;
    let mut result = Injected::new(
        params,
        json!({"private_dns": DNS_FAILURE_HOST, "was": {"mode": mode, "specifier": host}}),
    );
    result.warnings.push(
        "DNS failures start once Android re-validates the network, usually within a few seconds"
            .into(),
    );
    Ok(result)
}

/// A pair of numbers from the console, if it reported them.
type Pair = Option<(u64, u64)>;

/// `(upload, download)` kbit/s and `(min, max)` latency ms from `network status`.
fn parse_network_status(text: &str) -> (Pair, Pair) {
    let number = |label: &str| {
        text.lines()
            .find(|line| line.contains(label))
            .and_then(|line| line.split(':').nth(1))
            .and_then(|value| value.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
    };
    let speed = number("download speed")
        .zip(number("upload speed"))
        .map(|(down, up)| (up / 1000, down / 1000));
    let latency = number("minimum latency").zip(number("maximum latency"));
    (speed, latency)
}

async fn network_speed(ctx: &mut Ctx<'_>, profile: SpeedProfile) -> Result<Injected> {
    let status = device::emu(ctx.serial, &["network", "status"]).await?;
    let (speed, _) = parse_network_status(&status);
    let back = match speed {
        Some((up, down)) if up > 0 && down > 0 => format!("{up}:{down}"),
        _ => "full".into(),
    };
    let params = json!({"profile": profile.name()});
    ctx.recorder
        .record(
            &params,
            vec![RestoreStep::Emu {
                args: vec!["network".into(), "speed".into(), back.clone()],
                describe: format!("set the emulator link speed back to {back}"),
            }],
        )
        .await?;
    device::emu(ctx.serial, &["network", "speed", &profile.name()]).await?;
    let now = device::emu(ctx.serial, &["network", "status"]).await?;
    Ok(Injected::new(
        params,
        json!({"link_kbps": parse_network_status(&now).0.map(|(up, down)| json!({"up": up, "down": down})), "was": back}),
    ))
}

async fn network_latency(ctx: &mut Ctx<'_>, profile: LatencyProfile) -> Result<Injected> {
    let status = device::emu(ctx.serial, &["network", "status"]).await?;
    let (_, latency) = parse_network_status(&status);
    let back = match latency {
        Some((min, max)) if max > 0 => format!("{min}:{max}"),
        _ => "none".into(),
    };
    let params = json!({"profile": profile.name()});
    ctx.recorder
        .record(
            &params,
            vec![RestoreStep::Emu {
                args: vec!["network".into(), "delay".into(), back.clone()],
                describe: format!("set the emulator link latency back to {back}"),
            }],
        )
        .await?;
    device::emu(ctx.serial, &["network", "delay", &profile.name()]).await?;
    let now = device::emu(ctx.serial, &["network", "status"]).await?;
    Ok(Injected::new(
        params,
        json!({"latency_ms": parse_network_status(&now).1.map(|(min, max)| json!({"min": min, "max": max})), "was": back}),
    ))
}

// ── interruptions ─────────────────────────────────────────────────────────

fn valid_number(number: &str) -> Result<()> {
    if number.is_empty()
        || number.len() > 20
        || !number.chars().all(|c| c.is_ascii_digit() || c == '+')
    {
        return Err(error::invalid_param(format!(
            "{number:?} is not a phone number"
        )));
    }
    Ok(())
}

async fn incoming_call(ctx: &mut Ctx<'_>, number: &str) -> Result<Injected> {
    valid_number(number)?;
    let params = json!({"number": number});
    ctx.recorder
        .record(
            &params,
            vec![RestoreStep::Emu {
                args: vec!["gsm".into(), "cancel".into(), number.into()],
                describe: format!("hang up the call from {number}"),
            }],
        )
        .await?;
    device::emu(ctx.serial, &["gsm", "call", number]).await?;
    verify(
        ctx.serial,
        "dumpsys telephony.registry | grep mCallState",
        "mCallState=1",
    )
    .await?;
    Ok(Injected::new(params, json!({"call_state": "ringing"})))
}

async fn sms(ctx: &mut Ctx<'_>, from: &str, text: &str) -> Result<Injected> {
    valid_number(from)?;
    if text.is_empty() || text.contains('\n') {
        return Err(error::invalid_param("--text must be one non-empty line"));
    }
    device::emu(ctx.serial, &["sms", "send", from, text]).await?;
    Ok(Injected::new(
        json!({"from": from, "text_chars": text.chars().count()}),
        json!({"delivered_to_modem": true}),
    ))
}

async fn screen_off(ctx: &mut Ctx<'_>) -> Result<Injected> {
    let check = "dumpsys power | grep -m1 mWakefulness=";
    let prior = device::read_ok(ctx.serial, check).await?;
    if prior.contains("Asleep") || prior.contains("Dozing") {
        return Err(error::no_effect("the screen is already off"));
    }
    let params = json!({});
    ctx.recorder
        .record(
            &params,
            vec![verified_step(
                "input keyevent KEYCODE_WAKEUP; wm dismiss-keyguard",
                "wake the screen and dismiss a non-secure keyguard",
                check,
                "Awake",
            )],
        )
        .await?;
    device::run_ok(ctx.serial, "input keyevent KEYCODE_SLEEP").await?;
    verify(ctx.serial, check, "Asleep").await?;
    Ok(Injected::new(params, json!({"wakefulness": "Asleep"})))
}

async fn notification_shade(ctx: &mut Ctx<'_>, panel: Panel) -> Result<Injected> {
    let expand = match panel {
        Panel::Notifications => "cmd statusbar expand-notifications",
        Panel::QuickSettings => "cmd statusbar expand-settings",
    };
    let params = json!({"panel": panel.name()});
    ctx.recorder
        .record(
            &params,
            vec![shell_step("cmd statusbar collapse", "collapse the shade")],
        )
        .await?;
    device::run_ok(ctx.serial, expand).await?;
    verify(
        ctx.serial,
        "dumpsys window | grep -m1 mCurrentFocus",
        "NotificationShade",
    )
    .await?;
    Ok(Injected::new(
        params,
        json!({"focused_window": "NotificationShade"}),
    ))
}

async fn emulator_crash(ctx: &mut Ctx<'_>, relaunch: bool, cold_boot: bool) -> Result<Injected> {
    let avd = crate::device::target::avd_name(ctx.serial)
        .await
        .ok_or_else(|| error::requires_emulator("emulator-crash"))?;
    // Release this command's hold on the device while it can still answer;
    // after the kill there is no device to release it on.
    crate::runtime::release_for_passive_wait().await?;
    // `emu kill` stops the virtual machine at once: the guest gets no
    // shutdown, as in a host crash or power cut.
    device::emu(ctx.serial, &["kill"]).await?;
    let mut gone = false;
    for _ in 0..40 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let online = crate::device::adb::list_devices_with_state()
            .await
            .map(|devices| devices.iter().any(|(serial, _)| serial == ctx.serial))
            .unwrap_or(false);
        if !online {
            gone = true;
            break;
        }
    }
    if !gone {
        bail!(
            "{} is still listed by adb after the emulator was killed",
            ctx.serial
        );
    }
    let mut observed = json!({"avd": avd, "killed": true, "relaunched": false});
    let mut result = Injected::new(
        json!({"relaunch": relaunch, "cold_boot": cold_boot}),
        json!({}),
    );
    if relaunch {
        let log = crate::device::target::launch_avd(&avd, cold_boot)?;
        observed["relaunched"] = json!(true);
        observed["emulator_log"] = json!(log.display().to_string());
        result
            .next_actions
            .push(format!("shadowdroid -d {} connect", ctx.serial));
        result.warnings.push(
            "the emulator is booting; `connect` waits for it and restarts the ShadowDroid server"
                .into(),
        );
    } else {
        result.warnings.push(format!(
            "the emulator is gone; start AVD {avd} again (or re-run with --relaunch)"
        ));
    }
    result.observed = observed;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_outputs_parse() {
        assert_eq!(
            parse_wm("Physical size: 1080x2424\nOverride size: 900x2000"),
            (Some("1080x2424".into()), Some("900x2000".into()))
        );
        assert_eq!(
            parse_wm("Physical density: 420"),
            (Some("420".into()), None)
        );
        assert_eq!(
            parse_app_locales("Locales for io.x for user 0 are [ar-EG, en-US]"),
            "ar-EG,en-US"
        );
        assert_eq!(parse_app_locales("Locales for io.x for user 0 are []"), "");
        let status = "Current network status:\n  download speed:      14400 bits/s (1.8 KB/s)\n  upload speed:        14400 bits/s (1.8 KB/s)\n  minimum latency:  35 ms\n  maximum latency:  200 ms";
        assert_eq!(
            parse_network_status(status),
            (Some((14, 14)), Some((35, 200)))
        );
        assert_eq!(bucket_name("40"), "rare");
    }
}
