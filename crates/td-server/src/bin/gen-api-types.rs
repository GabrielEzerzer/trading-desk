//! Writes the GUI's TypeScript API types to `gui/shared/src/api-types.ts`.

use std::process::ExitCode;

use td_server::api::ts;

fn main() -> ExitCode {
    let path = ts::output_path();
    let written = path
        .parent()
        .map_or(Ok(()), std::fs::create_dir_all)
        .and_then(|()| std::fs::write(&path, ts::render()));
    match written {
        Ok(()) => {
            eprintln!("wrote {}", ts::OUTPUT);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {}: {e}", path.display());
            ExitCode::FAILURE
        }
    }
}
