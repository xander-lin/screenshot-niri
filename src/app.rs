use std::error::Error;

use crate::cli::Command;

pub fn run() -> Result<(), Box<dyn Error>> {
    let config = crate::config::Config::load();
    match Command::parse()? {
        Command::Screenshot(args) if args.help => {
            print!("{}", crate::cli::HELP);
            Ok(())
        }
        Command::Screenshot(args) => run_normal(args, config),
        Command::ClipboardProvider(args) => crate::clipboard::serve_path(&args.path, args.mode),
    }
}

fn run_normal(args: crate::cli::Args, config: crate::config::Config) -> Result<(), Box<dyn Error>> {
    // Both captures are still needed — the user can press `H` during the drag
    // — but only one of them is stored whole: the session keeps the
    // without-cursor capture and reduces the other to the pixels they differ by
    // (the cursor overlay; measured 0.07–0.13% of a 1920x1080 output).
    let frozen_outputs = crate::wayland::screencopy::capture_outputs_with_cursor()?;
    let frozen_outputs_without_cursor = crate::wayland::screencopy::capture_outputs_without_cursor()?;
    let mut session = crate::wayland::selection::SelectionSession::with_frozen_pair(
        &frozen_outputs,
        &frozen_outputs_without_cursor,
        config.border_width,
    )?;
    // The session has its own copies (one frame plus a cursor patch, and the
    // overlay buffers it sized from them), so these two full-frame sets are dead
    // weight while the user drags — 33 MB at 1920x1080. Dropping them here also
    // makes the `H` choice read from the session instead of from a second
    // source of truth that could disagree with what was displayed.
    drop(frozen_outputs);
    drop(frozen_outputs_without_cursor);
    match session.run_selection()? {
        crate::wayland::selection::SelectionOutcome::Selected(viewport) => {
            let captures = session.frozen_captures_for_output();
            // The foreground ends at the release: close the overlays before
            // any save work, so the frozen screen disappears now and the
            // compositing, PNG encode and clipboard handshake all run as
            // background work with the UI already gone.
            session.close()?;
            let output_path = args.output_path.clone();
            let encode = std::thread::spawn(move || -> Result<(), String> {
                let (width, height) =
                    viewport.capture_size(&captures).map_err(|e| e.to_string())?;
                let frame = crate::image::composite_captured_region(
                    &viewport.capture_regions(&captures),
                    width,
                    height,
                    &captures,
                )
                .map_err(|e| e.to_string())?;
                crate::image::write_png(&output_path, &frame).map_err(|e| e.to_string())
            });
            // Armed while the encode thread runs: the provider only
            // advertises the path and reads the file at paste time, so it
            // does not need the PNG to exist yet. The encode is joined, not
            // detached — a killed writer would leave a truncated PNG.
            let clipboard =
                crate::clipboard::serve_path_detached(&args.output_path, args.clipboard_mode);
            encode
                .join()
                .map_err(|_| "png encode thread panicked".to_string())
                .and_then(|result| result)?;
            clipboard?;
            println!("saved {}", args.output_path.display());
            Ok(())
        }
        crate::wayland::selection::SelectionOutcome::Cancelled => {
            session.close()?;
            Err("selection cancelled".into())
        }
    }
}
