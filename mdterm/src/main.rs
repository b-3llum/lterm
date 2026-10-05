fn main() -> std::process::ExitCode {
    mdterm::run("mdterm", std::env::args().skip(1).collect())
}
