//! Optional stdin command reader for `shadowdroid watch`.
//!
//! Each line is either:
//!   - JSON: {"cmd":"tap","id":5}
//!   - shorthand: `tap 5`, `back`, `launch com.foo`, `swipe 100 1500 100 200`
//!
//! Mirrors the `parse_command` function from the legacy `movi` CLI so existing
//! piped scripts (and the `movi` skill) keep working.

#![allow(dead_code)]

use anyhow::{anyhow, bail, Result};
use serde_json::Value;

pub fn parse_command(line: &str) -> Result<Value> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return Ok(Value::Null);
    }
    if line.starts_with('{') {
        return Ok(serde_json::from_str(line)?);
    }

    let parts = shell_words(line);
    let Some((cmd, args)) = parts.split_first() else {
        return Ok(Value::Null);
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();

    match cmd.as_str() {
        "tap" | "tap_and_wait" => {
            if args.len() == 1 {
                return Ok(json_obj(
                    &[("cmd", cmd), ("id", args[0])],
                    &[("id", parse_i32(args[0])?)],
                ));
            }
            if args.len() == 2 {
                return Ok(serde_json::json!({
                    "cmd": cmd,
                    "x": parse_i32(args[0])?,
                    "y": parse_i32(args[1])?,
                }));
            }
            bail!("{cmd} takes <id> or <x> <y>");
        }
        "double_tap" => {
            require_len(cmd, &args, 2)?;
            Ok(
                serde_json::json!({"cmd":"double_tap","x":parse_i32(args[0])?,"y":parse_i32(args[1])?}),
            )
        }
        "long_tap" => {
            if args.len() < 2 {
                bail!("long_tap takes <x> <y> [duration]");
            }
            Ok(serde_json::json!({
                "cmd": "long_tap",
                "x": parse_i32(args[0])?,
                "y": parse_i32(args[1])?,
                "duration": parse_f64_opt(args.get(2), 0.6)?,
            }))
        }
        "swipe" | "drag" => {
            if args.len() < 4 {
                bail!("{cmd} takes <x1> <y1> <x2> <y2> [duration]");
            }
            Ok(serde_json::json!({
                "cmd": cmd,
                "from": [parse_i32(args[0])?, parse_i32(args[1])?],
                "to": [parse_i32(args[2])?, parse_i32(args[3])?],
                "duration": parse_f64_opt(args.get(4), if cmd == "drag" { 0.5 } else { 0.2 })?,
            }))
        }
        "tap_text" | "tap_rid" | "tap_desc" | "tap_text_and_wait" | "tap_rid_and_wait"
        | "tap_desc_and_wait" => Ok(serde_json::json!({"cmd": cmd, "value": args.join(" ")})),
        "back" | "home" | "menu" => Ok(serde_json::json!({"cmd":"key","name":cmd})),
        "key" => {
            require_len(cmd, &args, 1)?;
            Ok(serde_json::json!({"cmd":"key","name":args[0]}))
        }
        "text" => Ok(serde_json::json!({"cmd":"text","value":args.join(" ")})),
        "launch" | "stop" | "app_clear" | "app_info" => {
            require_len(cmd, &args, 1)?;
            Ok(serde_json::json!({"cmd":cmd,"package":args[0]}))
        }
        "app_wait" => {
            require_len_at_least(cmd, &args, 1)?;
            Ok(serde_json::json!({
                "cmd": "app_wait",
                "package": args[0],
                "timeout": parse_f64_opt(args.get(1), 20.0)?,
            }))
        }
        "wait_activity" => {
            require_len_at_least(cmd, &args, 1)?;
            Ok(serde_json::json!({
                "cmd": "wait_activity",
                "name": args[0],
                "timeout": parse_f64_opt(args.get(1), 10.0)?,
            }))
        }
        "screenshot" => Ok(serde_json::json!({"cmd":"screenshot","path":args.first()})),
        "shell" => Ok(serde_json::json!({"cmd":"shell","value":args.join(" ")})),
        "screen_on" | "screen_off" | "wakeup" | "unlock" => Ok(serde_json::json!({"cmd":cmd})),
        "orientation" => {
            if let Some(value) = args.first() {
                Ok(serde_json::json!({"cmd":"set_orientation","value":value}))
            } else {
                Ok(serde_json::json!({"cmd":"orientation"}))
            }
        }
        "clipboard" => {
            if args.is_empty() {
                Ok(serde_json::json!({"cmd":"clipboard"}))
            } else {
                Ok(serde_json::json!({"cmd":"set_clipboard","value":args.join(" ")}))
            }
        }
        "notifications" => Ok(serde_json::json!({"cmd":"open_notification"})),
        "quick_settings" => Ok(serde_json::json!({"cmd":"open_quick_settings"})),
        "xpath" | "xpath_tap" => Ok(serde_json::json!({"cmd":cmd,"query":args.join(" ")})),
        "toast" => Ok(serde_json::json!({
            "cmd": "toast",
            "wait": parse_f64_opt(args.first(), 5.0)?,
        })),
        "swipe_ext" => {
            require_len_at_least(cmd, &args, 1)?;
            Ok(serde_json::json!({
                "cmd": "swipe_ext",
                "direction": args[0],
                "scale": parse_f64_opt(args.get(1), 0.9)?,
                "duration": parse_f64_opt(args.get(2), 0.2)?,
            }))
        }
        "open_url" => {
            require_len(cmd, &args, 1)?;
            Ok(serde_json::json!({"cmd":"open_url","url":args[0]}))
        }
        "push" => {
            if args.len() < 2 {
                bail!("push takes <local> <remote> [mode]");
            }
            let mut out = serde_json::json!({"cmd":"push","local":args[0],"remote":args[1]});
            if let Some(mode) = args.get(2) {
                out["mode"] = serde_json::json!(u32::from_str_radix(mode, 8)?);
            }
            Ok(out)
        }
        "pull" => {
            if args.len() < 2 {
                bail!("pull takes <remote> <local>");
            }
            Ok(serde_json::json!({"cmd":"pull","remote":args[0],"local":args[1]}))
        }
        "add_watcher" | "remove_watcher" | "list_watchers" | "clear_watchers" => {
            bail!("{cmd} requires JSON form")
        }
        "permission_dialogs" => {
            require_len(cmd, &args, 1)?;
            Ok(serde_json::json!({"cmd":"permission_dialogs","policy":args[0]}))
        }
        "screen" => Ok(serde_json::json!({"cmd":"screen"})),
        "quit" | "exit" => Ok(serde_json::json!({"cmd":"quit"})),
        _ => bail!("unknown command: {cmd}"),
    }
}

fn shell_words(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars();
    let mut quote: Option<char> = None;
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), '\\') => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                }
            }
            (Some(_), c) => cur.push(c),
            (None, '"' | '\'') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            (None, c) => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn require_len(cmd: &str, args: &[&str], len: usize) -> Result<()> {
    if args.len() == len {
        Ok(())
    } else {
        bail!("{cmd} takes {len} argument(s)")
    }
}

fn require_len_at_least(cmd: &str, args: &[&str], len: usize) -> Result<()> {
    if args.len() >= len {
        Ok(())
    } else {
        bail!("{cmd} takes at least {len} argument(s)")
    }
}

fn parse_i32(value: &str) -> Result<i32> {
    value
        .parse::<i32>()
        .map_err(|e| anyhow!("{value:?} is not an integer: {e}"))
}

fn parse_f64_opt(value: Option<&&str>, default: f64) -> Result<f64> {
    value
        .map(|v| {
            v.parse::<f64>()
                .map_err(|e| anyhow!("{v:?} is not a number: {e}"))
        })
        .unwrap_or(Ok(default))
}

fn json_obj(strs: &[(&str, &str)], nums: &[(&str, i32)]) -> Value {
    let mut map = serde_json::Map::new();
    for (k, v) in strs {
        map.insert((*k).to_string(), Value::String((*v).to_string()));
    }
    for (k, v) in nums {
        map.insert((*k).to_string(), Value::Number((*v).into()));
    }
    Value::Object(map)
}

#[cfg(test)]
mod tests {
    use super::parse_command;

    #[test]
    fn parses_shorthand_tap_by_id() {
        let v = parse_command("tap 4").unwrap();
        assert_eq!(v["cmd"], "tap");
        assert_eq!(v["id"], 4);
    }

    #[test]
    fn parses_quoted_text() {
        let v = parse_command("text \"hello world\"").unwrap();
        assert_eq!(v["cmd"], "text");
        assert_eq!(v["value"], "hello world");
    }

    #[test]
    fn parses_json_command() {
        let v = parse_command(r#"{"cmd":"key","name":"back"}"#).unwrap();
        assert_eq!(v["cmd"], "key");
        assert_eq!(v["name"], "back");
    }

    #[test]
    fn parses_permission_dialog_policy_command() {
        let v = parse_command("permission_dialogs deny").unwrap();
        assert_eq!(v["cmd"], "permission_dialogs");
        assert_eq!(v["policy"], "deny");
    }

    #[test]
    fn parses_tap_and_wait_by_id() {
        let v = parse_command("tap_and_wait 12").unwrap();
        assert_eq!(v["cmd"], "tap_and_wait");
        assert_eq!(v["id"], 12);
    }

    #[test]
    fn parses_tap_rid_and_wait() {
        let v = parse_command("tap_rid_and_wait main_tab_profile").unwrap();
        assert_eq!(v["cmd"], "tap_rid_and_wait");
        assert_eq!(v["value"], "main_tab_profile");
    }
}
