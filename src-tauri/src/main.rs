// Prevents additional console window on Windows in release, DO NOT REMOVE!!
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use clap::Parser;
use handy_app_lib::CliArgs;

fn main() {
    #[cfg(target_os = "linux")]
    {
        // DMABUF renderer causes crashes on various GPU/display server configurations
        // See: https://github.com/tauri-apps/tauri/issues/9394
        // Preserve explicit overrides so affected users can opt out with =0 (#2166).
        if std::env::var_os("WEBKIT_DISABLE_DMABUF_RENDERER").is_none() {
            std::env::set_var("WEBKIT_DISABLE_DMABUF_RENDERER", "1");
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Avoid overlay/capture layer crashes (#2049). Set before backend
        // initialization, preserving user overrides.
        handy_app_lib::set_env_default_unless(
            "VK_LOADER_LAYERS_DISABLE",
            "~implicit~",
            "HANDY_KEEP_VULKAN_IMPLICIT_LAYERS",
        );
        // NVIDIA coopmat2 shaders compile on every model load and the driver
        // cache does not keep them: the first transcription took ~48 s on an
        // RTX 3090 (then ~0.35 s). Without coopmat2 the first run takes <5 s
        // cold and ~0.4 s with a warm driver cache, steady speed equal or
        // better. Opt out with HANDY_KEEP_VULKAN_COOPMAT2=1.
        handy_app_lib::set_env_default_unless(
            "GGML_VK_DISABLE_COOPMAT2",
            "1",
            "HANDY_KEEP_VULKAN_COOPMAT2",
        );
    }

    // Transcription worker: runs transcribe.cpp in isolation and never touches
    // Tauri, the CLI, or single-instance handling. Checked after the env setup
    // above so the worker inherits it (e.g. the Vulkan layer opt-out).
    if handy_app_lib::engine_supervisor::is_worker_invocation() {
        std::process::exit(handy_app_lib::engine_supervisor::run_worker());
    }

    let cli_args = CliArgs::parse();
    handy_app_lib::run(cli_args)
}
