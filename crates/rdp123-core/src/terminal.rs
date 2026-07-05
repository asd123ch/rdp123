//! Launch SSH sessions in the user's chosen terminal.
//!
//! We never implement SSH ourselves: we start a terminal running `ssh …` and
//! let the user's keys / `~/.ssh/config` do the work. Different terminals are
//! launched differently — CLI terminals take the command directly, AppleScript
//! terminals (Terminal.app, iTerm2) are driven via `osascript`.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{bail, Context, Result};

use crate::profile::{Connection, Settings, TerminalKind};

/// Open an SSH session for `conn` using the configured terminal.
pub fn launch_ssh(settings: &Settings, conn: &Connection) -> Result<()> {
    conn.validate().context("invalid SSH connection")?;
    let target = if conn.username.is_empty() {
        conn.host.clone()
    } else {
        format!("{}@{}", conn.username, conn.host)
    };

    // Keep the target after `--` so a profile can never smuggle SSH options.
    let mut ssh = vec!["ssh".to_string()];
    if conn.port != 22 {
        ssh.push("-p".to_string());
        ssh.push(conn.port.to_string());
    }
    ssh.push("--".to_string());
    ssh.push(target);
    let ssh_line = shell_join(&ssh);

    let mut start_args = vec!["start".to_string(), "--".to_string()];
    start_args.extend(ssh.iter().cloned());

    match settings.terminal {
        TerminalKind::Kaku => spawn_cli_terminal("kaku", "Kaku", &start_args),
        TerminalKind::Wezterm => spawn_cli_terminal("wezterm", "WezTerm", &start_args),
        TerminalKind::Ghostty => spawn("open", |c| {
            c.args(["-na", "Ghostty", "--args", "-e"]).args(&ssh);
        }),
        TerminalKind::Alacritty => spawn("open", |c| {
            c.args(["-na", "Alacritty", "--args", "-e"]).args(&ssh);
        }),
        TerminalKind::Iterm => run_osascript(&iterm_script(&ssh_line)),
        TerminalKind::Terminal => run_osascript(&terminal_script(&ssh_line)),
        TerminalKind::Custom => run_custom(settings, conn, &ssh_line),
    }
}

/// Reap the launcher process in the background so it never lingers as a zombie.
fn reap(child: std::process::Child) {
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
}

fn spawn(program: &str, build: impl FnOnce(&mut Command)) -> Result<()> {
    let mut cmd = Command::new(program);
    build(&mut cmd);
    let child = cmd
        .spawn()
        .with_context(|| format!("failed to launch `{program}` — is it installed and on PATH?"))?;
    reap(child);
    Ok(())
}

/// Launch a CLI-first terminal (Kaku, WezTerm).
///
/// GUI apps inherit macOS's minimal PATH (no Homebrew, no app bundles), so the
/// bare binary name usually fails when RDP123 was started from Finder. Try the
/// PATH first, then well-known install locations, then fall back to opening
/// the app bundle — `open --args` hands the same CLI arguments to its binary.
fn spawn_cli_terminal(binary: &str, app: &str, args: &[String]) -> Result<()> {
    let mut candidates = vec![
        PathBuf::from(binary),
        PathBuf::from("/opt/homebrew/bin").join(binary),
        PathBuf::from("/usr/local/bin").join(binary),
        PathBuf::from(format!("/Applications/{app}.app/Contents/MacOS/{binary}")),
    ];
    if let Some(home) = std::env::var_os("HOME") {
        candidates.push(
            PathBuf::from(home).join(format!("Applications/{app}.app/Contents/MacOS/{binary}")),
        );
    }
    for candidate in &candidates {
        if let Ok(child) = Command::new(candidate).args(args).spawn() {
            reap(child);
            return Ok(());
        }
    }
    spawn("open", |c| {
        c.args(["-na", app, "--args"]).args(args);
    })
    .with_context(|| format!("failed to launch {app} — is it installed?"))
}

fn run_osascript(script: &str) -> Result<()> {
    let child = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .spawn()
        .context("failed to run osascript")?;
    reap(child);
    Ok(())
}

fn run_custom(settings: &Settings, conn: &Connection, ssh_line: &str) -> Result<()> {
    let template = settings.custom_terminal.as_deref().unwrap_or_default();
    if template.trim().is_empty() {
        bail!("no custom terminal command is configured (Settings → Terminal)");
    }
    let command = render_template(template, conn, ssh_line);
    let child = Command::new("/bin/sh")
        .arg("-c")
        .arg(&command)
        .spawn()
        .context("failed to run the custom terminal command")?;
    reap(child);
    Ok(())
}

fn applescript_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

fn shell_join(args: &[String]) -> String {
    args.iter()
        .map(|arg| shell_quote(arg))
        .collect::<Vec<_>>()
        .join(" ")
}

fn shell_quote(value: &str) -> String {
    if value.is_empty() {
        return "''".to_string();
    }
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn render_template(template: &str, conn: &Connection, ssh_line: &str) -> String {
    let replacements = [
        ("{ssh}", ssh_line.to_string()),
        ("{host}", shell_quote(&conn.host)),
        ("{port}", shell_quote(&conn.port.to_string())),
        ("{user}", shell_quote(&conn.username)),
    ];
    let mut output = String::with_capacity(template.len() + ssh_line.len());
    let mut rest = template;
    while !rest.is_empty() {
        if let Some((placeholder, replacement)) = replacements
            .iter()
            .find(|(placeholder, _)| rest.starts_with(placeholder))
        {
            output.push_str(replacement);
            rest = &rest[placeholder.len()..];
        } else {
            let ch = rest.chars().next().expect("rest is not empty");
            output.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    output
}

fn terminal_script(ssh_line: &str) -> String {
    let esc = applescript_escape(ssh_line);
    format!("tell application \"Terminal\"\nactivate\ndo script \"{esc}\"\nend tell")
}

fn iterm_script(ssh_line: &str) -> String {
    let esc = applescript_escape(ssh_line);
    format!(
        "tell application \"iTerm\"\nactivate\ncreate window with default profile command \"{esc}\"\nend tell"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::ConnectionKind;

    fn ssh_connection(host: &str, username: &str) -> Connection {
        let mut connection = Connection::new("SSH", ConnectionKind::Ssh);
        connection.host = host.to_string();
        connection.username = username.to_string();
        connection
    }

    #[test]
    fn shell_quote_neutralizes_metacharacters() {
        assert_eq!(
            shell_quote("server; touch /tmp/pwn"),
            "'server; touch /tmp/pwn'"
        );
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn custom_placeholders_are_quoted_once() {
        let connection = ssh_connection("host; touch /tmp/pwn", "alice");
        let rendered = render_template(
            "terminal -- {host} {user} {port} {ssh}",
            &connection,
            "'ssh' '--' 'alice@host; touch /tmp/pwn'",
        );
        assert_eq!(
            rendered,
            "terminal -- 'host; touch /tmp/pwn' 'alice' '22' 'ssh' '--' 'alice@host; touch /tmp/pwn'"
        );
    }

    #[test]
    fn profile_validation_rejects_option_injection() {
        let connection = ssh_connection("-oProxyCommand=evil", "");
        assert!(connection.validate().is_err());
    }
}
