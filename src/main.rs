use photoup2::ui;

fn main() -> glib::ExitCode {
    env_logger::init();
    log::info!("photoup2 {}", photoup2::VERSION);
    ui::run()
}
