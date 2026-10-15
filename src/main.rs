fn main() {
    let args: Vec<String> = std::env::args().collect();
    std::process::exit(prhub::cli::main(args));
}
