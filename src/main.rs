use std::process::ExitCode;

fn main() -> ExitCode {
    parhel::run(std::env::args_os())
}
