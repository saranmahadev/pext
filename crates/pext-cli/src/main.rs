use std::io::Write;

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    let env = pext_cli::Env::from_process();
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let code = pext_cli::run(argv, &env, &mut stdout.lock(), &mut stderr.lock());
    let _ = std::io::stdout().flush();
    std::process::exit(code);
}
