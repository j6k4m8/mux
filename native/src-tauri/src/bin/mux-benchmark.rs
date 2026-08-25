fn main() {
    match mux_native_lib::store::benchmark::run_native_benchmark() {
        Ok(report) => println!("{report}"),
        Err(error) => {
            eprintln!("Mux native SQLite benchmark failed: {error}");
            std::process::exit(1);
        }
    }
}
