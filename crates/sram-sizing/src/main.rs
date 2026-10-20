use std::{env, fs, process::ExitCode};

use argon_sram_sizing::{SramParams, render_argon_module};

fn usage(program: &str) -> String {
    format!(
        "usage: {program} <wmask-granularity> <mux-ratio> <num-words> <data-width> [function-name] [output.ar]"
    )
}

fn run() -> Result<(), String> {
    let mut args = env::args();
    let program = args.next().unwrap_or_else(|| "argon-sram-sizing".into());
    let values = (0..4)
        .map(|_| {
            args.next()
                .ok_or_else(|| usage(&program))?
                .parse::<i32>()
                .map_err(|error| format!("{error}\n{}", usage(&program)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let function_name = args.next().unwrap_or_else(|| "sram".to_owned());
    let output = args.next();
    if args.next().is_some() {
        return Err(usage(&program));
    }
    let params = SramParams::new(values[0], values[1], values[2], values[3])
        .map_err(|error| error.to_string())?;
    let source = render_argon_module(&function_name, &params.size());
    if let Some(path) = output {
        fs::write(&path, source).map_err(|error| format!("failed to write {path}: {error}"))?;
    } else {
        print!("{source}");
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}
