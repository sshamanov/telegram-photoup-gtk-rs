use photoup2::ui;

fn main() -> glib::ExitCode {
    // Normal operational logging by default: info shows app lifecycle, photo
    // loads, processing timings, and uploads. `RUST_LOG=debug` for the verbose
    // grammers/network trace; `RUST_LOG=error` to quiet it.
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("info"),
    )
    .format_timestamp_millis()
    .init();
    log::info!("photoup2 {} — starting", photoup2::VERSION);
    ui::run()
}
