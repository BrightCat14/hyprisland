mod audio;
mod bluez;
mod island;
mod layout;
mod media;
mod model;
mod render;
mod spring;
mod tray;

fn main() {
    if gtk::init().is_err() {
        eprintln!("hyprisland: failed to initialize GTK");
        std::process::exit(1);
    }
    let _window = island::build();
    gtk::main();
}
