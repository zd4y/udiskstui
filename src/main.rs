use app::App;
use color_eyre::Result;

mod app;
mod device;
mod errors;
mod tui;
mod udisks2;

fn main() -> Result<()> {
    errors::install_hooks()?;

    let mut app = App::new()?;
    let mut terminal = tui::init()?;
    let result = app.run(&mut terminal);
    tui::restore()?;
    result?;
    app.print_exit_mount_point();

    Ok(())
}
