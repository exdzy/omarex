//! `omarex` — fast native Linux GUI client for Superlogical's Rex and `tr3x` on Omarchy.

pub mod app;
pub mod bridge;
pub mod config;
pub mod host_manager;
pub(crate) mod media_viewer;
pub mod pane_store;
#[cfg(target_os = "linux")]
pub mod wayland_dnd;
pub mod workspace;

use std::path::PathBuf;

use anyhow::{Result, bail};
use tracing_subscriber::EnvFilter;

use crate::app::{GuiLaunchOptions, run_gui};
use crate::bridge::{HeadlessRenderOptions, render_headless_live_png_with_options};

#[derive(Debug, Clone, PartialEq)]
enum CliAction {
    Help,
    Version,
    RunGui(GuiLaunchOptions),
    HeadlessRender {
        out_path: PathBuf,
        options: HeadlessRenderOptions,
    },
}

fn main() -> Result<()> {
    init_tracing();

    let args: Vec<String> = std::env::args().skip(1).collect();
    match parse_cli_args(&args)? {
        CliAction::Help => {
            print_help();
            Ok(())
        }
        CliAction::Version => {
            let vt_ver = vt_engine::build_info()
                .map(|b| b.version)
                .unwrap_or_else(|_| "unknown".to_owned());
            println!(
                "omarex {} (Rex API rev {}, libghostty-vt {})",
                env!("CARGO_PKG_VERSION"),
                rex_proto::TARGET_API_REVISION,
                vt_ver
            );
            Ok(())
        }
        CliAction::HeadlessRender { out_path, options } => {
            let report = render_headless_live_png_with_options(&options, &out_path)?;
            println!(
                "omarex headless render complete: {} ({}x{} @{:.2}x) session={:?} windows={} ready_panes={} gpu={:?} frame={}us draws={} quads={} glyphs={}",
                report.output_path.display(),
                report.width,
                report.height,
                report.scale_factor,
                report.active_session_label,
                report.total_windows,
                report.ready_panes,
                report.frame_stats.adapter_name,
                report.frame_stats.total_us,
                report.frame_stats.draw_calls,
                report.frame_stats.quad_count,
                report.frame_stats.glyph_count,
            );
            Ok(())
        }
        CliAction::RunGui(opts) => run_gui(opts),
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

fn parse_cli_args(args: &[String]) -> Result<CliAction> {
    let mut session: Option<String> = None;
    let mut server: Option<String> = None;
    let mut headless_path: Option<PathBuf> = None;
    let mut headless_viewers: Vec<String> = Vec::new();
    let mut width: u32 = 1440;
    let mut height: u32 = 900;
    let mut scale: f32 = 1.0;
    let mut force_light = false;
    let mut app_id: Option<String> = None;
    let mut window_title: Option<String> = None;
    let mut working_directory: Option<String> = None;
    let mut command: Option<Vec<String>> = None;
    let mut wait_after_command = false;

    let mut i = 0usize;
    while i < args.len() {
        let arg = args[i].as_str();
        if let Some(val) = arg.strip_prefix("--server=") {
            server = Some(val.to_owned());
        } else if let Some(val) = arg.strip_prefix("--headless-render=") {
            headless_path = Some(PathBuf::from(val));
        } else if let Some(val) = arg.strip_prefix("--headless-viewer=") {
            for item in val.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                headless_viewers.push(item.to_owned());
            }
        } else if let Some(val) = arg.strip_prefix("--width=") {
            width = val
                .parse::<u32>()
                .map_err(|_| anyhow::anyhow!("invalid --width `{val}`"))?
                .clamp(320, 8192);
        } else if let Some(val) = arg.strip_prefix("--height=") {
            height = val
                .parse::<u32>()
                .map_err(|_| anyhow::anyhow!("invalid --height `{val}`"))?
                .clamp(240, 8192);
        } else if let Some(val) = arg.strip_prefix("--scale=") {
            scale = val
                .parse::<f32>()
                .map_err(|_| anyhow::anyhow!("invalid --scale `{val}`"))?
                .clamp(0.5, 4.0);
        } else if let Some(val) = arg
            .strip_prefix("--class=")
            .or_else(|| arg.strip_prefix("--app-id="))
        {
            app_id = Some(val.to_owned());
        } else if let Some(val) = arg.strip_prefix("--title=") {
            window_title = Some(val.to_owned());
        } else if let Some(val) = arg
            .strip_prefix("--working-directory=")
            .or_else(|| arg.strip_prefix("--dir="))
        {
            working_directory = Some(val.to_owned());
        } else {
            match arg {
                "-h" | "--help" => return Ok(CliAction::Help),
                "-V" | "--version" => return Ok(CliAction::Version),
                "-S" | "--server" => {
                    i += 1;
                    let Some(val) = args.get(i) else {
                        bail!("missing value for {arg}");
                    };
                    server = Some(val.clone());
                }
                "--headless-render" => {
                    i += 1;
                    let Some(val) = args.get(i) else {
                        bail!("missing output PNG path for --headless-render");
                    };
                    headless_path = Some(PathBuf::from(val));
                }
                "--headless-viewer" => {
                    i += 1;
                    let Some(val) = args.get(i) else {
                        bail!("missing path for --headless-viewer");
                    };
                    for item in val.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                        headless_viewers.push(item.to_owned());
                    }
                }
                "--width" => {
                    i += 1;
                    let Some(val) = args.get(i) else {
                        bail!("missing value for --width");
                    };
                    width = val
                        .parse::<u32>()
                        .map_err(|_| anyhow::anyhow!("invalid --width `{val}`"))?
                        .clamp(320, 8192);
                }
                "--height" => {
                    i += 1;
                    let Some(val) = args.get(i) else {
                        bail!("missing value for --height");
                    };
                    height = val
                        .parse::<u32>()
                        .map_err(|_| anyhow::anyhow!("invalid --height `{val}`"))?
                        .clamp(240, 8192);
                }
                "--scale" => {
                    i += 1;
                    let Some(val) = args.get(i) else {
                        bail!("missing value for --scale");
                    };
                    scale = val
                        .parse::<f32>()
                        .map_err(|_| anyhow::anyhow!("invalid --scale `{val}`"))?
                        .clamp(0.5, 4.0);
                }
                "--light" => {
                    force_light = true;
                }
                "--class" | "--app-id" => {
                    i += 1;
                    let Some(val) = args.get(i) else {
                        bail!("missing value for {arg}");
                    };
                    app_id = Some(val.clone());
                }
                "--title" => {
                    i += 1;
                    let Some(val) = args.get(i) else {
                        bail!("missing value for {arg}");
                    };
                    window_title = Some(val.clone());
                }
                "--working-directory" | "--dir" => {
                    i += 1;
                    let Some(val) = args.get(i) else {
                        bail!("missing value for {arg}");
                    };
                    working_directory = Some(val.clone());
                }
                "--wait-after-command" | "--hold" => {
                    wait_after_command = true;
                }
                "-e" | "--command" => {
                    let rest = &args[i + 1..];
                    if rest.is_empty() {
                        bail!("missing command after {arg}");
                    }
                    command = Some(rest.to_vec());
                    break;
                }
                "--" => {
                    if let Some(rest) = args.get(i + 1)
                        && session.is_none()
                    {
                        session = Some(rest.clone());
                    }
                    break;
                }
                other if other.starts_with('-') => {
                    bail!("unknown option `{other}` (run `omarex --help` for usage)");
                }
                positional => {
                    if session.is_some() {
                        bail!("unexpected extra argument `{positional}`");
                    }
                    session = Some(positional.to_owned());
                }
            }
        }
        i += 1;
    }

    if let Some(out_path) = headless_path {
        Ok(CliAction::HeadlessRender {
            out_path,
            options: HeadlessRenderOptions {
                server_override: server,
                session_filter: session,
                width,
                height,
                scale_factor: scale,
                force_light,
                viewer_paths: headless_viewers,
            },
        })
    } else {
        Ok(CliAction::RunGui(GuiLaunchOptions {
            server_override: server,
            preferred_session: session,
            force_light,
            app_id,
            window_title,
            working_directory,
            command,
            wait_after_command,
        }))
    }
}

fn print_help() {
    println!(
        "\
omarex {} — native Linux Vulkan GUI client for Superlogical's Rex and tr3x on Omarchy

USAGE:
    omarex [OPTIONS] [SESSION]
    omarex [OPTIONS] -e <COMMAND> [ARGS...]

ARGS:
    [SESSION]                       Optional initial session name or ID (e.g. \"T3 Code\")

OPTIONS:
    -S, --server <ENDPOINT>         Override Rex server endpoint (unix://, https://, ssh://)
        --class, --app-id <ID>      Set Wayland app_id / X11 WM_CLASS (default: omarex)
        --title <TITLE>             Set pinned window title
        --working-directory, --dir  Set initial working directory for shells / -e commands
        --wait-after-command, --hold Keep window open after -e command exits
    -e <COMMAND> [ARGS...]          Run command in a clean standalone ephemeral Rex session
        --headless-render <PATH>    Render live session offscreen on Vulkan to PNG path
        --headless-viewer <PATH>    Open file/image/video in right-hand viewer for headless PNG (repeatable)
        --width <PX>                Headless render width in physical pixels (default: 1440)
        --height <PX>               Headless render height in physical pixels (default: 900)
        --scale <FACTOR>            Headless render HiDPI scale factor (default: 1.0)
        --light                     Force light theme palette (Alpine Milk / Omarchy light)
    -V, --version                   Print version, Rex API revision, and libghostty-vt version
    -h, --help                      Print help information",
        env!("CARGO_PKG_VERSION")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(items: &[&str]) -> Vec<String> {
        items.iter().map(|v| (*v).to_owned()).collect()
    }

    #[test]
    fn cli_parses_gui_session_and_server_override() {
        let act = parse_cli_args(&s(&[
            "--server",
            "unix:///run/user/1000/rex/server.sock",
            "T3 Code",
        ]))
        .expect("parse gui args");
        assert_eq!(
            act,
            CliAction::RunGui(GuiLaunchOptions {
                server_override: Some("unix:///run/user/1000/rex/server.sock".into()),
                preferred_session: Some("T3 Code".into()),
                force_light: false,
                app_id: None,
                window_title: None,
                working_directory: None,
                command: None,
                wait_after_command: false,
            })
        );
    }

    #[test]
    fn cli_parses_xdg_terminal_exec_flags_and_command() {
        let act = parse_cli_args(&s(&[
            "--class=org.omarchy.btop",
            "--title=Omarchy",
            "--working-directory=/home/user/projects",
            "--wait-after-command",
            "-e",
            "btop",
            "--utf-force",
        ]))
        .expect("parse xdg-terminal-exec args");
        assert_eq!(
            act,
            CliAction::RunGui(GuiLaunchOptions {
                server_override: None,
                preferred_session: None,
                force_light: false,
                app_id: Some("org.omarchy.btop".into()),
                window_title: Some("Omarchy".into()),
                working_directory: Some("/home/user/projects".into()),
                command: Some(s(&["btop", "--utf-force"])),
                wait_after_command: true,
            })
        );
    }

    #[test]
    fn cli_parses_headless_render_with_dimensions_and_light_flag() {
        let act = parse_cli_args(&s(&[
            "--headless-render",
            "/tmp/out.png",
            "--headless-viewer",
            "README.md:12",
            "--headless-viewer=/tmp/a.png,/tmp/b.mp4",
            "--width",
            "1920",
            "--height",
            "1080",
            "--scale",
            "1.5",
            "--light",
            "T3 Code",
        ]))
        .expect("parse headless args");
        match act {
            CliAction::HeadlessRender { out_path, options } => {
                assert_eq!(out_path, PathBuf::from("/tmp/out.png"));
                assert_eq!(options.width, 1920);
                assert_eq!(options.height, 1080);
                assert!((options.scale_factor - 1.5).abs() < f32::EPSILON);
                assert!(options.force_light);
                assert_eq!(options.session_filter.as_deref(), Some("T3 Code"));
                assert_eq!(
                    options.viewer_paths,
                    vec!["README.md:12", "/tmp/a.png", "/tmp/b.mp4"]
                );
            }
            other => panic!("expected HeadlessRender, got {other:?}"),
        }
    }

    #[test]
    fn cli_handles_help_version_and_rejects_unknown_flags() {
        assert_eq!(parse_cli_args(&s(&["--help"])).unwrap(), CliAction::Help);
        assert_eq!(
            parse_cli_args(&s(&["--version"])).unwrap(),
            CliAction::Version
        );
        assert!(parse_cli_args(&s(&["--bogus-flag"])).is_err());
    }
}
