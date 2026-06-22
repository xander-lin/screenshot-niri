use std::error::Error;

use crate::cli::Command;

pub fn run() -> Result<(), Box<dyn Error>> {
    match Command::parse()? {
        Command::Screenshot(args) if args.help => {
            print!("{}", crate::cli::HELP);
            Ok(())
        }
        Command::Screenshot(args) => run_normal(args),
        Command::ClipboardProvider(args) => crate::clipboard::serve_path(&args.path, args.mode),
    }
}

fn run_normal(args: crate::cli::Args) -> Result<(), Box<dyn Error>> {
    let frozen_outputs = crate::wayland::screencopy::capture_outputs(true)?;
    let mut session = crate::wayland::selection::SelectionSession::with_frozen(&frozen_outputs)?;
    let viewport = match session.run_selection()? {
        crate::wayland::selection::SelectionOutcome::Selected(viewport) => viewport,
        crate::wayland::selection::SelectionOutcome::Cancelled => {
            session.close()?;
            return Err("selection cancelled".into());
        }
    };
    session.close()?;
    let (width, height) = viewport.capture_size()?;
    let frame = crate::image::composite_captured_region(
        &viewport.capture_regions(),
        width,
        height,
        &frozen_outputs,
    )?;
    crate::image::write_png(&args.output_path, &frame)?;
    crate::clipboard::serve_path_detached(&args.output_path, args.clipboard_mode)?;
    println!("saved {}", args.output_path.display());
    Ok(())
}

