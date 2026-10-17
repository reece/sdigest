use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(sdigest::cli::run(std::env::args_os()))
}
