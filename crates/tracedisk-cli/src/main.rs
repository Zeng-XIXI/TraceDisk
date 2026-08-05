use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use tracedisk_core::inspect_image;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("error: {message}");
            eprintln!();
            print_usage();
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut arguments = env::args_os().skip(1);
    let Some(command) = arguments.next() else {
        return Err("missing command".into());
    };

    if command == "--help" || command == "-h" || command == "help" {
        print_usage();
        return Ok(());
    }

    if command != "inspect" {
        return Err(format!("unknown command: {}", command.to_string_lossy()));
    }

    let Some(image_path) = arguments.next().map(PathBuf::from) else {
        return Err("inspect requires an image path".into());
    };

    let mut json = false;
    for argument in arguments {
        match argument.to_string_lossy().as_ref() {
            "--json" => json = true,
            unknown => return Err(format!("unknown option: {unknown}")),
        }
    }

    let report = inspect_image(&image_path).map_err(|error| error.to_string())?;
    if json {
        println!("{}", report.to_json_pretty());
    } else {
        print!("{}", report.to_human_readable());
    }

    Ok(())
}

fn print_usage() {
    eprintln!("TraceDisk - read-only camera card recovery toolkit");
    eprintln!();
    eprintln!("USAGE:");
    eprintln!("    tracedisk inspect <IMAGE> [--json]");
    eprintln!();
    eprintln!("The current milestone accepts regular image files only.");
}
